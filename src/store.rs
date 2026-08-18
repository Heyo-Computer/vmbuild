//! Content-addressed store for built ext4 images.
//!
//! Layout under the store root:
//!
//! ```text
//!   blobs/<key>.ext4     the image, mode 0444
//!   meta/<key>.json      what it is, when it was last used
//!   tmp/<rand>           staging; renamed into place (same fs => atomic)
//!   locks/<key>.lock     flock, so concurrent builds of one key serialize
//! ```
//!
//! Installing into heyvm's catalog uses a **hardlink**, not a copy. The
//! catalog is 42G apparent / 19G actual and undeduplicated on a disk that
//! runs near full, and nothing in heyvm mutates a catalog `.ext4` in place --
//! `inject_authorized_keys_via_debugfs`, `grow_ext4_image` and the workspace
//! `data.ext4` all operate on per-VM copies. Cross-filesystem installs
//! (`MVM_DATA_DIR` can point anywhere) fall back to a copy.

use crate::error::{Error, Result};
use fs2::FileExt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum InstallKind {
    Link,
    Copy,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub key: String,
    pub size_bytes: u64,
    pub actual_bytes: u64,
    pub diff_ids: Vec<String>,
    /// Unix seconds. Touched on every cache hit so GC can evict least-recently
    /// used entries.
    pub last_used: u64,
    pub created: u64,
}

pub struct Store {
    root: PathBuf,
}

/// Guard holding an exclusive flock for one key.
pub struct KeyLock(fs::File);

impl Drop for KeyLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Store {
    /// Default root: `$VMBUILD_STORE`, else `$MVM_DATA_DIR/vmbuild`, else
    /// `~/.heyo/vmbuild` -- alongside heyvm's data so the catalog is on the
    /// same filesystem and hardlinks work.
    pub fn default_root() -> PathBuf {
        if let Ok(p) = std::env::var("VMBUILD_STORE") {
            return PathBuf::from(p);
        }
        if let Ok(p) = std::env::var("MVM_DATA_DIR") {
            return PathBuf::from(p).join("vmbuild");
        }
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join(".heyo")
            .join("vmbuild")
    }

    pub fn open(root: &Path) -> Result<Self> {
        for d in ["blobs", "meta", "tmp", "locks"] {
            fs::create_dir_all(root.join(d))?;
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob(&self, key: &str) -> PathBuf {
        self.root.join("blobs").join(format!("{key}.ext4"))
    }
    fn meta(&self, key: &str) -> PathBuf {
        self.root.join("meta").join(format!("{key}.json"))
    }

    /// Serialize concurrent builds of the same key. Two different Dockerfiles
    /// that happen to produce the same rootfs will contend here, which is
    /// correct -- they want the same blob.
    pub fn lock(&self, key: &str) -> Result<KeyLock> {
        let p = self.root.join("locks").join(format!("{key}.lock"));
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(p)?;
        FileExt::lock_exclusive(&f)?;
        Ok(KeyLock(f))
    }

    /// The stored image for `key`, if present. Touches `last_used`.
    pub fn get(&self, key: &str) -> Option<PathBuf> {
        let b = self.blob(key);
        if !b.exists() {
            return None;
        }
        if let Ok(mut e) = self.entry(key) {
            e.last_used = now_secs();
            let _ = self.write_meta(&e);
        }
        Some(b)
    }

    pub fn entry(&self, key: &str) -> Result<Entry> {
        let text = fs::read_to_string(self.meta(key))?;
        serde_json::from_str(&text)
            .map_err(|e| Error::Io(io::Error::new(io::ErrorKind::InvalidData, e)))
    }

    fn write_meta(&self, e: &Entry) -> Result<()> {
        let tmp = self.root.join("tmp").join(format!("{}.json", e.key));
        fs::write(&tmp, serde_json::to_vec_pretty(e).unwrap_or_default())?;
        fs::rename(&tmp, self.meta(&e.key))?;
        Ok(())
    }

    /// Move a freshly built image into the store under `key`.
    ///
    /// `src` must already be on the store's filesystem (build straight into
    /// `staging_path`) so the rename is atomic; otherwise it is copied.
    pub fn put(&self, key: &str, src: &Path, diff_ids: &[String]) -> Result<PathBuf> {
        let dest = self.blob(key);
        match fs::rename(src, &dest) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                fs::copy(src, &dest)?;
                let _ = fs::remove_file(src);
            }
            Err(e) => return Err(Error::Io(e)),
        }
        // Read-only: a stored blob is shared by every catalog name that links
        // to it, so an in-place edit would corrupt all of them at once.
        let mut perms = fs::metadata(&dest)?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o444);
        fs::set_permissions(&dest, perms)?;

        let md = fs::metadata(&dest)?;
        let now = now_secs();
        self.write_meta(&Entry {
            key: key.to_string(),
            size_bytes: md.len(),
            actual_bytes: md.blocks() * 512,
            diff_ids: diff_ids.to_vec(),
            last_used: now,
            created: now,
        })?;
        Ok(dest)
    }

    /// A path on the store's filesystem to build into, so `put` is a rename.
    pub fn staging_path(&self, key: &str) -> PathBuf {
        self.root.join("tmp").join(format!("{key}.building"))
    }

    /// Publish a stored image under a human-readable path (heyvm's catalog).
    ///
    /// Hardlink when possible; copy across filesystems. Always staged through
    /// `<dest>.part` + rename, so a crash never leaves a truncated `.ext4`
    /// that a sandbox create would happily try to boot.
    pub fn install(&self, key: &str, dest: &Path) -> Result<InstallKind> {
        let blob = self.blob(key);
        if !blob.exists() {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no stored image for key {key}"),
            )));
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let part = dest.with_extension("ext4.part");
        let _ = fs::remove_file(&part);

        let kind = match fs::hard_link(&blob, &part) {
            Ok(()) => InstallKind::Link,
            Err(e)
                if e.raw_os_error() == Some(libc::EXDEV)
                    || e.kind() == io::ErrorKind::PermissionDenied =>
            {
                fs::copy(&blob, &part)?;
                // A copy is a private file; make it writable like heyvm's
                // existing catalog entries so nothing downstream is surprised.
                let mut p = fs::metadata(&part)?.permissions();
                std::os::unix::fs::PermissionsExt::set_mode(&mut p, 0o644);
                fs::set_permissions(&part, p)?;
                InstallKind::Copy
            }
            Err(e) => return Err(Error::Io(e)),
        };
        fs::rename(&part, dest)?;
        Ok(kind)
    }

    pub fn list(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for e in fs::read_dir(self.root.join("meta"))?.flatten() {
            if let Ok(text) = fs::read_to_string(e.path())
                && let Ok(entry) = serde_json::from_str::<Entry>(&text)
            {
                out.push(entry);
            }
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.last_used));
        Ok(out)
    }

    /// Evict least-recently-used entries until the store fits `policy`.
    ///
    /// Liveness: an entry whose blob has `st_nlink > 1` is referenced by an
    /// installed catalog name and is never evicted. That signal only works
    /// once callers install via [`Store::install`]; until heyvm adopts it,
    /// entries it copied out look unreferenced, so keep `max_bytes` generous.
    pub fn gc(&self, policy: &GcPolicy) -> Result<GcReport> {
        let mut entries = self.list()?;
        // Oldest first.
        entries.sort_by_key(|e| e.last_used);

        let mut total: u64 = entries.iter().map(|e| e.actual_bytes).sum();
        let now = now_secs();
        let mut report = GcReport {
            removed: Vec::new(),
            kept_linked: 0,
            freed_bytes: 0,
            total_before: total,
            dry_run: policy.dry_run,
        };

        for e in entries {
            let over_size = policy.max_bytes.is_some_and(|m| total > m);
            let too_old = policy
                .keep_secs
                .is_some_and(|k| now.saturating_sub(e.last_used) > k);
            if !over_size && !too_old {
                continue;
            }
            let blob = self.blob(&e.key);
            if let Ok(md) = fs::metadata(&blob)
                && md.nlink() > 1
            {
                report.kept_linked += 1;
                continue; // still installed somewhere
            }
            if !policy.dry_run {
                let _ = fs::remove_file(&blob);
                let _ = fs::remove_file(self.meta(&e.key));
            }
            total = total.saturating_sub(e.actual_bytes);
            report.freed_bytes += e.actual_bytes;
            report.removed.push(e.key);
        }
        Ok(report)
    }
}

#[derive(Debug, Clone, Default)]
pub struct GcPolicy {
    pub max_bytes: Option<u64>,
    pub keep_secs: Option<u64>,
    pub dry_run: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct GcReport {
    pub removed: Vec<String>,
    pub kept_linked: usize,
    pub freed_bytes: u64,
    pub total_before: u64,
    pub dry_run: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        (d, s)
    }

    fn put_dummy(s: &Store, key: &str, bytes: &[u8]) -> PathBuf {
        let staging = s.staging_path(key);
        fs::write(&staging, bytes).unwrap();
        s.put(key, &staging, &["sha256:x".into()]).unwrap()
    }

    #[test]
    fn put_then_get_roundtrips() {
        let (_d, s) = store();
        assert!(s.get("k1").is_none());
        put_dummy(&s, "k1", b"hello");
        let got = s.get("k1").expect("present after put");
        assert_eq!(fs::read(got).unwrap(), b"hello");
        assert_eq!(s.entry("k1").unwrap().size_bytes, 5);
    }

    #[test]
    fn stored_blobs_are_read_only() {
        let (_d, s) = store();
        let p = put_dummy(&s, "k", b"x");
        let mode = fs::metadata(&p).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o444,
            "a shared blob must not be writable in place"
        );
    }

    #[test]
    fn install_hardlinks_within_one_filesystem() {
        let (d, s) = store();
        put_dummy(&s, "k", b"payload");
        let dest = d.path().join("catalog/debian.ext4");
        assert_eq!(s.install("k", &dest).unwrap(), InstallKind::Link);
        assert_eq!(fs::read(&dest).unwrap(), b"payload");
        // Same inode => the catalog costs no extra bytes.
        assert_eq!(
            fs::metadata(&dest).unwrap().ino(),
            fs::metadata(s.blob("k")).unwrap().ino()
        );
    }

    #[test]
    fn install_leaves_no_part_file_behind() {
        let (d, s) = store();
        put_dummy(&s, "k", b"payload");
        let dest = d.path().join("c/img.ext4");
        s.install("k", &dest).unwrap();
        assert!(!dest.with_extension("ext4.part").exists());
    }

    #[test]
    fn install_is_idempotent() {
        let (d, s) = store();
        put_dummy(&s, "k", b"payload");
        let dest = d.path().join("c/img.ext4");
        s.install("k", &dest).unwrap();
        s.install("k", &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"payload");
    }

    #[test]
    fn install_missing_key_errors() {
        let (d, s) = store();
        assert!(s.install("nope", &d.path().join("x.ext4")).is_err());
    }

    #[test]
    fn gc_respects_size_cap_and_evicts_oldest_first() {
        let (_d, s) = store();
        put_dummy(&s, "old", &[0u8; 8192]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        put_dummy(&s, "new", &[0u8; 8192]);

        let r = s
            .gc(&GcPolicy {
                max_bytes: Some(8192),
                ..Default::default()
            })
            .unwrap();
        assert!(r.removed.contains(&"old".to_string()), "report={r:?}");
        assert!(s.get("new").is_some(), "newest entry must survive");
    }

    #[test]
    fn gc_never_evicts_an_installed_entry() {
        let (d, s) = store();
        put_dummy(&s, "linked", &[0u8; 8192]);
        s.install("linked", &d.path().join("catalog/x.ext4"))
            .unwrap();

        let r = s
            .gc(&GcPolicy {
                max_bytes: Some(0), // evict everything it is allowed to
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.kept_linked, 1);
        assert!(r.removed.is_empty());
        assert!(s.get("linked").is_some());
    }

    #[test]
    fn gc_dry_run_reports_without_deleting() {
        let (_d, s) = store();
        put_dummy(&s, "k", &[0u8; 8192]);
        let r = s
            .gc(&GcPolicy {
                max_bytes: Some(0),
                dry_run: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.removed, vec!["k".to_string()]);
        assert!(s.get("k").is_some(), "dry run must not delete");
    }

    #[test]
    fn gc_age_policy() {
        let (_d, s) = store();
        put_dummy(&s, "k", &[0u8; 4096]);
        // keep_secs=0 => anything older than 0s is evictable.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let r = s
            .gc(&GcPolicy {
                keep_secs: Some(0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.removed, vec!["k".to_string()]);
    }

    #[test]
    fn lock_is_exclusive_across_handles() {
        let (d, s) = store();
        let _held = s.lock("k").unwrap();
        // A second Store on the same root must block; assert via try_lock on a
        // fresh handle to the same lock file.
        let p = d.path().join("locks/k.lock");
        let f = fs::OpenOptions::new().write(true).open(p).unwrap();
        assert!(
            FileExt::try_lock_exclusive(&f).is_err(),
            "second lock should not be grantable while the first is held"
        );
    }
}
