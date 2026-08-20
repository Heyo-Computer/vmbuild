//! ZFS-backed storage: a dataset per image, a clone per materialization.
//!
//! Enabled with `--features zfs`. **Experimental, and root-only** — on Linux
//! `zfs allow` cannot delegate the `mount` permission, and `create`, `clone`
//! and `destroy` all require it, so delegation does not make this work
//! unprivileged. That contradicts vmbuild's usual no-privilege property, which
//! is why it is off by default.
//!
//! The model is the one containerd's ZFS snapshotter, LXD and Proxmox all use:
//! commit an image as a dataset plus a snapshot, and clone that snapshot per
//! instance. A file inside a dataset rather than a zvol, deliberately: the ext4
//! writer does `File::create` + `set_len` and seeks a sparse file, which fails
//! EINVAL on a zvol node, and zvols additionally default to a 16K
//! `volblocksize` against 4K ext4 blocks.
//!
//! Everything shells out to `zfs(8)`. Every Rust ZFS binding is stale — the
//! newest release across `libzetta`, `zfs-core` and `libzfs` is 2023 — and
//! containerd's snapshotter shells out too.
//!
//! ## Lifecycle, and why it matters
//!
//! A clone pins its origin snapshot: while any clone exists, `zfs destroy` on
//! the image dataset returns EBUSY. So a materialization that is never released
//! leaks twice — the clone itself, and the blob it makes unreclaimable.
//! [`StorageBackend::release`] is not optional bookkeeping here; it is what
//! makes the store collectable. `gc` reads the clone graph
//! (`zfs list -o name,origin`) and refuses to touch a pinned image rather than
//! failing halfway.

use crate::error::{Error, Result};
use crate::store::{
    Entry, GcPolicy, GcReport, InstallKind, KeyLock, Materialization, StorageBackend,
};
use std::path::{Path, PathBuf};

/// The `zfs(8)` command surface, behind a seam so the logic is testable on a
/// machine with no ZFS.
pub trait Zfs: Send + Sync {
    /// Run `zfs` with these arguments, returning stdout.
    fn run(&self, argv: &[&str]) -> Result<String>;
}

/// Shells out to the real `zfs` binary.
pub struct SystemZfs;

impl Zfs for SystemZfs {
    fn run(&self, argv: &[&str]) -> Result<String> {
        let out = std::process::Command::new("zfs")
            .args(argv)
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Error::ToolMissing { tool: "zfs" }
                } else {
                    Error::Io(e)
                }
            })?;
        if !out.status.success() {
            return Err(Error::ToolFailed {
                tool: "zfs",
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Does this error mean "the object is pinned", rather than a real failure?
///
/// ZFS reports a dataset with dependent clones as busy. Treating that as
/// success is how a blob becomes invisible garbage, so it is classified.
pub fn is_busy(e: &Error) -> bool {
    match e {
        Error::ToolFailed { stderr, .. } => {
            let s = stderr.to_ascii_lowercase();
            s.contains("dataset is busy")
                || s.contains("has dependent clones")
                || s.contains("filesystem has children")
        }
        _ => false,
    }
}

/// The image file inside each dataset.
const IMAGE: &str = "image.ext4";
/// The snapshot a clone is taken from.
const READY: &str = "ready";

pub struct ZfsBackend<Z: Zfs> {
    zfs: Z,
    /// Parent dataset, e.g. `tank/vmbuild`.
    dataset: String,
    /// Where that dataset is mounted, so files can be read and written
    /// normally once ZFS has done the dataset work.
    mountpoint: PathBuf,
}

impl<Z: Zfs> ZfsBackend<Z> {
    pub fn new(zfs: Z, dataset: impl Into<String>, mountpoint: impl Into<PathBuf>) -> Self {
        Self {
            zfs,
            dataset: dataset.into(),
            mountpoint: mountpoint.into(),
        }
    }

    fn blob_ds(&self, key: &str) -> String {
        format!("{}/blobs/{key}", self.dataset)
    }
    fn clone_ds(&self, token: &str) -> String {
        format!("{}/clones/{token}", self.dataset)
    }
    fn blob_dir(&self, key: &str) -> PathBuf {
        self.mountpoint.join("blobs").join(key)
    }
    fn blob_file(&self, key: &str) -> PathBuf {
        self.blob_dir(key).join(IMAGE)
    }
    fn meta_path(&self, key: &str) -> PathBuf {
        self.mountpoint.join("meta").join(format!("{key}.json"))
    }

    /// A stable, filesystem-safe name for a clone of a given destination.
    ///
    /// Derived from the destination path so `release(dest)` can find the clone
    /// again without a side table.
    fn clone_token(dest: &Path) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(dest.as_os_str().as_encoded_bytes());
        format!("{:x}", h.finalize())[..32].to_string()
    }

    /// `dataset -> origin` for everything under our root, in one call.
    fn clone_graph(&self) -> Result<Vec<(String, String)>> {
        let out = self
            .zfs
            .run(&["list", "-H", "-r", "-o", "name,origin", &self.dataset])?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let mut it = l.split('\t');
                let name = it.next()?.trim().to_string();
                let origin = it.next()?.trim().to_string();
                (origin != "-" && !origin.is_empty()).then_some((name, origin))
            })
            .collect())
    }

    /// Image datasets that a live clone currently pins.
    fn pinned(&self) -> Result<Vec<String>> {
        Ok(self
            .clone_graph()?
            .into_iter()
            .map(|(_, origin)| origin.split('@').next().unwrap_or_default().to_string())
            .collect())
    }
}

impl<Z: Zfs> StorageBackend for ZfsBackend<Z> {
    fn root(&self) -> &Path {
        &self.mountpoint
    }

    fn lock(&self, key: &str) -> Result<KeyLock> {
        // A plain file lock under the mountpoint: the dataset work is what
        // differs between backends, not the mutual exclusion.
        KeyLock::acquire(&self.mountpoint.join("locks"), key)
    }

    fn staging_path(&self, key: &str) -> PathBuf {
        self.mountpoint.join("tmp").join(format!("{key}.building"))
    }

    fn put(&self, key: &str, src: &Path, diff_ids: &[String]) -> Result<PathBuf> {
        // One dataset per image, so it can be snapshotted and cloned.
        self.zfs.run(&["create", "-p", &self.blob_ds(key)])?;
        let dest = self.blob_file(key);
        std::fs::rename(src, &dest).or_else(|_| {
            std::fs::copy(src, &dest).map(|_| ()).inspect(|_| {
                let _ = std::fs::remove_file(src);
            })
        })?;

        // Snapshot *after* the data is in place; the snapshot is what clones
        // are taken from, so it must capture a complete image.
        std::fs::File::open(&dest).and_then(|f| f.sync_all()).ok();
        self.zfs
            .run(&["snapshot", &format!("{}@{READY}", self.blob_ds(key))])?;
        self.zfs.run(&["set", "readonly=on", &self.blob_ds(key)])?;

        let md = std::fs::metadata(&dest)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = Entry {
            key: key.to_string(),
            size_bytes: md.len(),
            actual_bytes: {
                use std::os::unix::fs::MetadataExt;
                md.blocks() * 512
            },
            diff_ids: diff_ids.to_vec(),
            last_used: now,
            created: now,
        };
        let meta = self.meta_path(key);
        if let Some(p) = meta.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&meta, serde_json::to_vec_pretty(&entry).unwrap_or_default())?;
        Ok(dest)
    }

    fn get(&self, key: &str) -> Option<PathBuf> {
        self.blob_file(key).exists().then(|| self.blob_file(key))
    }

    fn entry(&self, key: &str) -> Result<Entry> {
        let text = std::fs::read_to_string(self.meta_path(key))?;
        serde_json::from_str(&text)
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
    }

    /// Read-only publication is still a plain hardlink: nothing about a shared
    /// read-only name benefits from being its own dataset.
    fn install(&self, key: &str, dest: &Path) -> Result<InstallKind> {
        let blob = self.blob_file(key);
        if !blob.exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no stored image for key {key}"),
            )));
        }
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        let _ = std::fs::remove_file(dest);
        match std::fs::hard_link(&blob, dest) {
            Ok(()) => Ok(InstallKind::Link),
            Err(_) => {
                std::fs::copy(&blob, dest)?;
                Ok(InstallKind::Copy)
            }
        }
    }

    /// `zfs clone` of the image's `@ready` snapshot — the point of this backend.
    fn materialize(&self, key: &str, dest: &Path) -> Result<Materialization> {
        if !self.blob_file(key).exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no stored image for key {key}"),
            )));
        }
        let token = Self::clone_token(dest);
        let clone = self.clone_ds(&token);
        let snap = format!("{}@{READY}", self.blob_ds(key));

        // Re-materializing over the same destination is legitimate; drop any
        // previous clone first so this is not an error.
        let _ = self.zfs.run(&["destroy", "-r", &clone]);
        self.zfs
            .run(&["clone", "-o", "readonly=off", &snap, &clone])?;

        // A clone inherits the origin's modes, and the origin is deliberately
        // read-only. The caller asked for something writable.
        let cloned_file = self.mountpoint.join("clones").join(&token).join(IMAGE);
        if cloned_file.exists() {
            let mut p = std::fs::metadata(&cloned_file)?.permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut p, 0o644);
            std::fs::set_permissions(&cloned_file, p)?;
            let _ = std::fs::remove_file(dest);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // The clone lives in its own dataset; give the caller the path they
            // asked for as a link to it.
            std::fs::hard_link(&cloned_file, dest)?;
        }

        // A clone shares every block with its origin at creation.
        Ok(Materialization::Cloned { bytes_written: 0 })
    }

    /// Destroy the clone behind `dest`. Without this the store is permanently
    /// unreclaimable, because each clone pins its origin snapshot.
    fn release(&self, dest: &Path) -> Result<()> {
        let clone = self.clone_ds(&Self::clone_token(dest));
        let _ = std::fs::remove_file(dest);
        match self.zfs.run(&["destroy", "-r", &clone]) {
            Ok(_) => Ok(()),
            // Already gone is success: a caller reaping after a crash cannot
            // know whether the clone survived.
            Err(Error::ToolFailed { ref stderr, .. }) if stderr.contains("does not exist") => {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn list(&self) -> Result<Vec<Entry>> {
        let dir = self.mountpoint.join("meta");
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if let Ok(t) = std::fs::read_to_string(e.path())
                    && let Ok(entry) = serde_json::from_str::<Entry>(&t)
                {
                    out.push(entry);
                }
            }
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.last_used));
        Ok(out)
    }

    fn gc(&self, policy: &GcPolicy) -> Result<GcReport> {
        let mut entries = self.list()?;
        entries.sort_by_key(|e| e.last_used);
        let pinned = self.pinned()?;

        let mut total: u64 = entries.iter().map(|e| e.actual_bytes).sum();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut report = GcReport {
            total_before: total,
            dry_run: policy.dry_run,
            ..Default::default()
        };

        for e in entries {
            let over = policy.max_bytes.is_some_and(|m| total > m);
            let old = policy
                .keep_secs
                .is_some_and(|k| now.saturating_sub(e.last_used) > k);
            if !over && !old {
                continue;
            }
            // Ask ZFS what is pinned rather than maintaining a refcount: the
            // kernel already tracks it exactly.
            if pinned.iter().any(|p| p == &self.blob_ds(&e.key)) {
                report.kept_busy.push(e.key);
                continue;
            }
            if !policy.dry_run {
                match self.zfs.run(&["destroy", "-r", &self.blob_ds(&e.key)]) {
                    Ok(_) => {}
                    Err(err) if is_busy(&err) => {
                        report.kept_busy.push(e.key);
                        continue;
                    }
                    Err(err) => return Err(err),
                }
                // Only once the dataset is confirmed gone.
                let _ = std::fs::remove_file(self.meta_path(&e.key));
            }
            total = total.saturating_sub(e.actual_bytes);
            report.freed_bytes += e.actual_bytes;
            report.removed.push(e.key);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records every `zfs` invocation and replays canned output.
    ///
    /// Proves argv construction, parsing and error classification. Proves
    /// nothing about whether a real cloned dataset yields a bootable rootfs --
    /// that needs the `#[ignore]` suite on real ZFS.
    #[derive(Default)]
    pub struct FakeZfs {
        pub calls: Mutex<Vec<String>>,
        pub responses: Mutex<Vec<Result<String>>>,
    }

    impl FakeZfs {
        fn with(responses: Vec<Result<String>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Zfs for FakeZfs {
        fn run(&self, argv: &[&str]) -> Result<String> {
            self.calls.lock().unwrap().push(argv.join(" "));
            let mut r = self.responses.lock().unwrap();
            if r.is_empty() {
                Ok(String::new())
            } else {
                r.remove(0)
            }
        }
    }

    fn busy(msg: &str) -> Error {
        Error::ToolFailed {
            tool: "zfs",
            status: "exit status: 1".into(),
            stderr: msg.into(),
        }
    }

    #[test]
    fn busy_errors_are_classified_not_swallowed() {
        assert!(is_busy(&busy("cannot destroy 'tank/x': dataset is busy")));
        assert!(is_busy(&busy(
            "cannot destroy 'tank/x': filesystem has dependent clones"
        )));
        assert!(!is_busy(&busy(
            "cannot open 'tank/x': dataset does not exist"
        )));
    }

    #[test]
    fn materialize_clones_the_ready_snapshot() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("blobs/abc")).unwrap();
        std::fs::write(d.path().join("blobs/abc/image.ext4"), b"x").unwrap();

        let be = ZfsBackend::new(FakeZfs::default(), "tank/vmbuild", d.path());
        be.materialize("abc", &d.path().join("vm.ext4")).unwrap();

        let calls = be.zfs.calls();
        let clone = calls
            .iter()
            .find(|c| c.starts_with("clone"))
            .expect("no clone call");
        assert!(
            clone.contains("tank/vmbuild/blobs/abc@ready"),
            "cloned the wrong snapshot: {clone}"
        );
        assert!(
            clone.contains("readonly=off"),
            "clone would inherit the origin's read-only mode: {clone}"
        );
    }

    #[test]
    fn release_destroys_the_clone_for_that_destination() {
        let d = tempfile::tempdir().unwrap();
        let be = ZfsBackend::new(FakeZfs::default(), "tank/vmbuild", d.path());
        let dest = d.path().join("vm.ext4");

        be.release(&dest).unwrap();
        let token = ZfsBackend::<FakeZfs>::clone_token(&dest);
        assert_eq!(
            be.zfs.calls(),
            vec![format!("destroy -r tank/vmbuild/clones/{token}")]
        );
    }

    #[test]
    fn release_of_a_missing_clone_is_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        let be = ZfsBackend::new(
            FakeZfs::with(vec![Err(busy(
                "cannot open 'tank/x': dataset does not exist",
            ))]),
            "tank/vmbuild",
            d.path(),
        );
        be.release(&d.path().join("vm.ext4")).unwrap();
    }

    #[test]
    fn clone_token_is_stable_and_destination_specific() {
        let a = Path::new("/run/vm-a/rootfs.ext4");
        let b = Path::new("/run/vm-b/rootfs.ext4");
        assert_eq!(
            ZfsBackend::<FakeZfs>::clone_token(a),
            ZfsBackend::<FakeZfs>::clone_token(a),
            "release could not find the clone materialize made"
        );
        assert_ne!(
            ZfsBackend::<FakeZfs>::clone_token(a),
            ZfsBackend::<FakeZfs>::clone_token(b)
        );
    }

    #[test]
    fn gc_refuses_to_destroy_an_image_a_clone_still_pins() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("meta")).unwrap();
        let entry = Entry {
            key: "abc".into(),
            size_bytes: 10,
            actual_bytes: 10,
            diff_ids: vec![],
            last_used: 0,
            created: 0,
        };
        std::fs::write(
            d.path().join("meta/abc.json"),
            serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();

        // `zfs list` reports a live clone whose origin is that image.
        let listing = "tank/vmbuild\t-\n\
                       tank/vmbuild/blobs/abc\t-\n\
                       tank/vmbuild/clones/deadbeef\ttank/vmbuild/blobs/abc@ready\n";
        let be = ZfsBackend::new(
            FakeZfs::with(vec![Ok(listing.into())]),
            "tank/vmbuild",
            d.path(),
        );

        let r = be
            .gc(&GcPolicy {
                max_bytes: Some(0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.kept_busy, vec!["abc".to_string()], "{r:?}");
        assert!(r.removed.is_empty());
        assert!(
            !be.zfs.calls().iter().any(|c| c.starts_with("destroy")),
            "GC tried to destroy a pinned image: {:?}",
            be.zfs.calls()
        );
    }

    #[test]
    fn gc_destroys_an_unpinned_image_then_its_metadata() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("meta")).unwrap();
        let entry = Entry {
            key: "abc".into(),
            size_bytes: 10,
            actual_bytes: 10,
            diff_ids: vec![],
            last_used: 0,
            created: 0,
        };
        std::fs::write(
            d.path().join("meta/abc.json"),
            serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();

        let be = ZfsBackend::new(
            FakeZfs::with(vec![Ok("tank/vmbuild\t-\n".into())]),
            "tank/vmbuild",
            d.path(),
        );
        let r = be
            .gc(&GcPolicy {
                max_bytes: Some(0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.removed, vec!["abc".to_string()]);
        assert!(
            be.zfs
                .calls()
                .iter()
                .any(|c| c == "destroy -r tank/vmbuild/blobs/abc")
        );
        assert!(!d.path().join("meta/abc.json").exists());
    }

    /// A busy dataset must keep its metadata, or the image becomes invisible
    /// garbage that no later GC can find.
    #[test]
    fn gc_keeps_metadata_when_destroy_reports_busy() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("meta")).unwrap();
        let entry = Entry {
            key: "abc".into(),
            size_bytes: 10,
            actual_bytes: 10,
            diff_ids: vec![],
            last_used: 0,
            created: 0,
        };
        std::fs::write(
            d.path().join("meta/abc.json"),
            serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();

        let be = ZfsBackend::new(
            FakeZfs::with(vec![
                Ok("tank/vmbuild\t-\n".into()),                   // no clones listed
                Err(busy("cannot destroy 'x': dataset is busy")), // but destroy says otherwise
            ]),
            "tank/vmbuild",
            d.path(),
        );
        let r = be
            .gc(&GcPolicy {
                max_bytes: Some(0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.kept_busy, vec!["abc".to_string()]);
        assert!(
            d.path().join("meta/abc.json").exists(),
            "metadata deleted for a dataset that still exists"
        );
    }
}
