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
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Sparse copy
// ---------------------------------------------------------------------------

/// Copy `src` to `dest` preserving holes, returning the bytes actually written.
///
/// `std::fs::copy` uses `copy_file_range`, which does **not** preserve holes:
/// measured here, a 200 MiB-apparent / 4 MiB-actual file becomes 200 MiB
/// actual. Rootfs images are extremely sparse -- one catalog image is 20 GiB
/// apparent for 582 MiB of data -- so a densifying copy turns a 582 MiB image
/// into 20 GiB of writes. (`cp` looks fine only because coreutils does this
/// same hole walk itself.)
///
/// Linux only. Elsewhere this falls back to `fs::copy`; correctness is
/// identical, only the sparseness is lost.
#[cfg(target_os = "linux")]
fn sparse_copy(src: &Path, dest: &Path) -> Result<u64> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;

    // SEEK_DATA / SEEK_HOLE. Not exposed by std::io::SeekFrom.
    const SEEK_DATA: i32 = 3;
    const SEEK_HOLE: i32 = 4;

    let mut s = fs::File::open(src)?;
    // OpenZFS before 2.2.2 reports stale hole information for a file with
    // dirty data -- the December 2023 "cp corrupts files" bug. vmbuild writes
    // a blob and copies it moments later, which is exactly that shape, so sync
    // before trusting SEEK_DATA. Best-effort: fsync on an O_RDONLY fd is legal
    // on Linux but not worth failing the copy over.
    let _ = s.sync_all();

    let len = s.metadata()?.len();
    let mut d = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644) // never inherit a daemon's umask
        .open(dest)?;
    // Establish the full length up front so a hole at EOF survives -- a
    // data-extent-only copy would otherwise truncate it away.
    d.set_len(len)?;

    let mut written = 0u64;
    let mut off = 0i64;
    let mut buf = vec![0u8; 1 << 20];
    while (off as u64) < len {
        // Next byte of data at or after `off`; ENXIO means only holes remain.
        let start = unsafe { libc::lseek(s.as_raw_fd(), off, SEEK_DATA) };
        if start < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            return Err(Error::Io(e));
        }
        let end = unsafe { libc::lseek(s.as_raw_fd(), start, SEEK_HOLE) };
        if end < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }

        s.seek(SeekFrom::Start(start as u64))?;
        d.seek(SeekFrom::Start(start as u64))?;
        let mut remaining = (end - start) as u64;
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            let n = s.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            d.write_all(&buf[..n])?;
            written += n as u64;
            remaining -= n as u64;
        }
        off = end;
    }
    d.sync_all()?;
    Ok(written)
}

#[cfg(not(target_os = "linux"))]
fn sparse_copy(src: &Path, dest: &Path) -> Result<u64> {
    Ok(fs::copy(src, dest)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum InstallKind {
    Link,
    Copy,
}

/// Attempt `FICLONE`. `Ok(None)` means the filesystem declined and the caller
/// should copy; errors are only for genuinely unexpected failures.
///
/// Deliberately always attempted rather than predicted from the filesystem
/// type: support is per (source, destination) pair -- EXDEV across mounts, and
/// OpenZFS refuses cross-dataset clones on recordsize or encryption mismatch.
#[cfg(target_os = "linux")]
fn try_ficlone(src: &Path, dest: &Path) -> Result<Option<u64>> {
    use std::os::fd::AsRawFd;
    // _IOW(0x94, 9, int) -- linux/fs.h
    const FICLONE: libc::c_ulong = 0x4004_9409;

    let s = fs::File::open(src)?;
    let d = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(dest)?;

    let rc = unsafe { libc::ioctl(d.as_raw_fd(), FICLONE, s.as_raw_fd()) };
    if rc == 0 {
        let bytes = d.metadata()?.blocks() * 512;
        return Ok(Some(bytes));
    }
    let e = io::Error::last_os_error();
    drop(d);
    let _ = fs::remove_file(dest);
    match e.raw_os_error() {
        // "this filesystem/pair cannot clone" -- every one of these means fall
        // back, not fail.
        Some(libc::EOPNOTSUPP)
        | Some(libc::ENOTTY)
        | Some(libc::EXDEV)
        | Some(libc::EINVAL)
        | Some(libc::EPERM)
        | Some(libc::EACCES) => Ok(None),
        _ => Err(Error::Io(e)),
    }
}

#[cfg(not(target_os = "linux"))]
fn try_ficlone(_src: &Path, _dest: &Path) -> Result<Option<u64>> {
    Ok(None)
}

/// How a writable copy was produced, and what it cost.
///
/// `#[non_exhaustive]` from the outset: a copy-on-write backend will add
/// variants, and downstream `match`es must not break when it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub enum Materialization {
    /// The filesystem shared the blocks (FICLONE): btrfs, XFS with reflink, or
    /// OpenZFS 2.2+ with `feature@block_cloning` and `zfs_bclone_enabled=1`.
    Cloned { bytes_written: u64 },
    /// Copied, skipping holes. `bytes_written` counts only real data, so a
    /// regression to a densifying copy is visible rather than silent.
    SparseCopy { bytes_written: u64 },
}

impl Materialization {
    pub fn bytes_written(self) -> u64 {
        match self {
            Materialization::Cloned { bytes_written }
            | Materialization::SparseCopy { bytes_written } => bytes_written,
        }
    }
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

impl KeyLock {
    /// Take an exclusive lock named `key` inside `dir`, creating `dir` if
    /// needed. Shared by every backend -- only the storage differs, not the
    /// mutual exclusion.
    pub fn acquire(dir: &Path, key: &str) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!("{key}.lock")))?;
        FileExt::lock_exclusive(&f)?;
        Ok(KeyLock(f))
    }
}

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
        KeyLock::acquire(&self.root.join("locks"), key)
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
                sparse_copy(src, &dest)?;
                let _ = fs::remove_file(src);
            }
            Err(e) => return Err(Error::Io(e)),
        }
        // Read-only: a stored blob is shared by every catalog name that links
        // to it, so an in-place edit would corrupt all of them at once.
        let mut perms = fs::metadata(&dest)?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o444);
        fs::set_permissions(&dest, perms)?;

        // Durability before measurement: `blocks()` can read 0 while the data
        // is still only in the page cache (ext4 delalloc, or a ZFS txg not yet
        // synced). A zero `actual_bytes` would make GC believe the entry
        // occupies nothing and never count it against a size budget.
        fs::File::open(&dest).and_then(|f| f.sync_all()).ok();
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

        // Already installed? Then there is nothing to do -- and doing it
        // anyway is actively harmful. `rename` between two links to the *same*
        // inode is defined by POSIX to "return successfully and perform no
        // other action", so the staging link would survive the rename, leaving
        // a stray `.part` that inflates the blob's link count. GC treats
        // `nlink > 1` as "still referenced", so that orphan would pin the blob
        // forever.
        if let (Ok(d), Ok(b)) = (fs::metadata(dest), fs::metadata(&blob))
            && d.ino() == b.ino()
            && d.dev() == b.dev()
        {
            return Ok(InstallKind::Link);
        }

        let part = dest.with_extension("ext4.part");
        let _ = fs::remove_file(&part);

        let kind = match fs::hard_link(&blob, &part) {
            Ok(()) => InstallKind::Link,
            Err(e)
                if e.raw_os_error() == Some(libc::EXDEV)
                    || e.kind() == io::ErrorKind::PermissionDenied =>
            {
                // A copy is a private file, and `sparse_copy` creates it 0644
                // -- writable like heyvm's existing catalog entries, so
                // nothing downstream is surprised by the blob's 0444.
                sparse_copy(&blob, &part)?;
                InstallKind::Copy
            }
            Err(e) => return Err(Error::Io(e)),
        };
        fs::rename(&part, dest)?;
        // Belt to the early-return's braces: if `part` and `dest` ever end up
        // as links to one inode, the rename above is a silent no-op.
        let _ = fs::remove_file(&part);
        Ok(kind)
    }

    /// Produce a **writable private** copy of a stored image at `dest`.
    ///
    /// Distinct from [`Store::install`], which hardlinks: a hardlink is right
    /// for read-only catalog names but hands out an alias of the shared 0444
    /// blob, so writing through it would corrupt every other name at once.
    /// This always yields an independent, writable file.
    ///
    /// Tries `FICLONE` first, so on a copy-on-write filesystem the copy is
    /// near-free; otherwise falls back to a hole-preserving copy. Either way
    /// the result reports the bytes actually written.
    ///
    /// Note this is a primitive, not a policy: it has no notion of leases or
    /// of who owns the result. Reclaiming `dest` is the caller's business.
    pub fn materialize(&self, key: &str, dest: &Path) -> Result<Materialization> {
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
        let _ = fs::remove_file(dest);

        if let Some(bytes) = try_ficlone(&blob, dest)? {
            return Ok(Materialization::Cloned {
                bytes_written: bytes,
            });
        }
        let bytes_written = sparse_copy(&blob, dest)?;
        Ok(Materialization::SparseCopy { bytes_written })
    }

    /// Reclaim a copy previously produced by [`Store::materialize`].
    ///
    /// On a POSIX filesystem this is just an unlink -- the interesting case is
    /// a backend where a materialization is a first-class object that pins the
    /// blob it came from (a ZFS clone holds its origin snapshot). Without an
    /// owner for that lifecycle, materializing leaks in two directions: the
    /// clones themselves, and the blobs they make undeletable. Callers should
    /// call this when a VM's disk is discarded.
    ///
    /// Idempotent: releasing something already gone is not an error.
    pub fn release(&self, dest: &Path) -> Result<()> {
        match fs::remove_file(dest) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
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

        // Measure now rather than trusting `Entry.actual_bytes`, which was
        // recorded at `put` time. On a filesystem that shares blocks, an
        // entry's real footprint changes underneath us.
        let live = |e: &Entry| -> u64 {
            fs::metadata(self.blob(&e.key))
                .map(|m| m.blocks() * 512)
                .unwrap_or(e.actual_bytes)
        };

        let mut total: u64 = entries.iter().map(&live).sum();
        let now = now_secs();
        let mut report = GcReport {
            removed: Vec::new(),
            kept_linked: 0,
            kept_busy: Vec::new(),
            freed_bytes: 0,
            total_before: total,
            dry_run: policy.dry_run,
            stopped_early: None,
            last_free: None,
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
            let claimed = live(&e);

            if !policy.dry_run {
                // Two-phase delete. Removing the metadata first (or blindly)
                // is how a blob becomes invisible garbage: it would vanish
                // from `list()` while still occupying the disk, so no future
                // GC could ever find it. A backend that refuses the removal --
                // ZFS returns EBUSY for a dataset with dependent clones -- must
                // therefore keep its metadata.
                match fs::remove_file(&blob) {
                    Ok(()) => {}
                    Err(e2) if e2.kind() == io::ErrorKind::NotFound => {}
                    Err(_) => {
                        report.kept_busy.push(e.key);
                        continue;
                    }
                }
                let _ = fs::remove_file(self.meta(&e.key));

                // Did the disk actually get smaller? Under block sharing the
                // answer can be "no", and then evicting harder does not help:
                // without this the size budget would evict the entire store
                // and still be over. Only meaningful for a size policy.
                if policy.max_bytes.is_some()
                    && claimed > 0
                    && let Some(freed) = self.freed_since(&mut report)
                    && freed * 4 < claimed
                {
                    report.stopped_early = Some(
                        "measured free space barely moved -- these blobs appear to \
                         share blocks with something else; stopping rather than \
                         evicting the whole store"
                            .into(),
                    );
                    report.removed.push(e.key);
                    report.freed_bytes += claimed;
                    break;
                }
            }
            total = total.saturating_sub(claimed);
            report.freed_bytes += claimed;
            report.removed.push(e.key);
        }
        Ok(report)
    }

    /// Bytes the filesystem reports as freed since the last call. `None` when
    /// `statvfs` is unavailable, in which case the caller simply does not apply
    /// the no-progress rule.
    fn freed_since(&self, report: &mut GcReport) -> Option<u64> {
        let now = free_bytes(&self.root)?;
        let prev = report.last_free.replace(now);
        prev.map(|p| now.saturating_sub(p))
    }
}

/// The operations a storage backend must provide.
///
/// `Store` (POSIX files, hardlinks, `FICLONE`) is the default and only
/// implementation compiled in unless the `zfs` feature is enabled. Every method
/// mirrors an existing inherent method on `Store`, and the impl delegates to
/// them -- inherent methods win Rust's method resolution, so existing callers
/// are entirely unaffected by this trait existing.
///
/// Object-safe on purpose: the CLI picks a backend at runtime.
pub trait StorageBackend: Send + Sync {
    /// Where this backend keeps its data.
    fn root(&self) -> &Path;
    /// Serialize concurrent work on one key.
    fn lock(&self, key: &str) -> Result<KeyLock>;
    /// A path on the backend's own storage to build into, so `put` is cheap.
    fn staging_path(&self, key: &str) -> PathBuf;
    fn put(&self, key: &str, src: &Path, diff_ids: &[String]) -> Result<PathBuf>;
    fn get(&self, key: &str) -> Option<PathBuf>;
    fn entry(&self, key: &str) -> Result<Entry>;
    /// Publish a *read-only shared* name for a stored image.
    fn install(&self, key: &str, dest: &Path) -> Result<InstallKind>;
    /// Produce a *writable private* copy.
    fn materialize(&self, key: &str, dest: &Path) -> Result<Materialization>;
    /// Reclaim something `materialize` produced. Idempotent.
    fn release(&self, dest: &Path) -> Result<()>;
    fn list(&self) -> Result<Vec<Entry>>;
    fn gc(&self, policy: &GcPolicy) -> Result<GcReport>;
}

impl StorageBackend for Store {
    fn root(&self) -> &Path {
        Store::root(self)
    }
    fn lock(&self, key: &str) -> Result<KeyLock> {
        Store::lock(self, key)
    }
    fn staging_path(&self, key: &str) -> PathBuf {
        Store::staging_path(self, key)
    }
    fn put(&self, key: &str, src: &Path, diff_ids: &[String]) -> Result<PathBuf> {
        Store::put(self, key, src, diff_ids)
    }
    fn get(&self, key: &str) -> Option<PathBuf> {
        Store::get(self, key)
    }
    fn entry(&self, key: &str) -> Result<Entry> {
        Store::entry(self, key)
    }
    fn install(&self, key: &str, dest: &Path) -> Result<InstallKind> {
        Store::install(self, key, dest)
    }
    fn materialize(&self, key: &str, dest: &Path) -> Result<Materialization> {
        Store::materialize(self, key, dest)
    }
    fn release(&self, dest: &Path) -> Result<()> {
        Store::release(self, dest)
    }
    fn list(&self) -> Result<Vec<Entry>> {
        Store::list(self)
    }
    fn gc(&self, policy: &GcPolicy) -> Result<GcReport> {
        Store::gc(self, policy)
    }
}

#[derive(Debug, Clone, Default)]
pub struct GcPolicy {
    pub max_bytes: Option<u64>,
    pub keep_secs: Option<u64>,
    pub dry_run: bool,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct GcReport {
    pub removed: Vec<String>,
    /// Entries skipped because something still links them.
    pub kept_linked: usize,
    /// Entries whose blob could not be removed -- e.g. a ZFS dataset with
    /// dependent clones (EBUSY). Their metadata is deliberately preserved so
    /// they stay visible to a later GC.
    pub kept_busy: Vec<String>,
    pub freed_bytes: u64,
    pub total_before: u64,
    pub dry_run: bool,
    /// Set when GC stopped because evicting was not actually freeing space.
    pub stopped_early: Option<String>,
    /// Crate-internal bookkeeping for the no-progress rule; not public API.
    #[serde(skip)]
    pub(crate) last_free: Option<u64>,
}

/// Free bytes on the filesystem holding `path`, via `statvfs`.
#[cfg(target_os = "linux")]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(target_os = "linux"))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
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

    /// A sparse file must survive the copy as a sparse file.
    ///
    /// This is the regression test for `copy_file_range`, which `std::fs::copy`
    /// uses and which silently densifies: rootfs images are extremely sparse
    /// (one catalog image is 20 GiB apparent for 582 MiB of data), so a
    /// densifying copy turns 582 MiB into 20 GiB of writes.
    #[test]
    #[cfg(target_os = "linux")]
    fn sparse_copy_preserves_holes_and_reports_real_bytes() {
        use std::io::{Seek, SeekFrom, Write};
        const APPARENT: u64 = 64 * 1024 * 1024;
        const CHUNK: usize = 1024 * 1024;

        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("sparse.img");
        {
            // 4 MiB of real data, in four chunks scattered through 64 MiB.
            let mut f = fs::File::create(&src).unwrap();
            f.set_len(APPARENT).unwrap();
            for i in 0..4u64 {
                f.seek(SeekFrom::Start(i * 15 * 1024 * 1024)).unwrap();
                f.write_all(&vec![b'D'; CHUNK]).unwrap();
            }
            f.sync_all().unwrap();
        }
        let src_md = fs::metadata(&src).unwrap();
        let src_actual = src_md.blocks() * 512;
        assert!(
            src_actual < APPARENT / 4,
            "fixture is not sparse: {src_actual} of {APPARENT}"
        );

        let dst = d.path().join("copy.img");
        let written = sparse_copy(&src, &dst).unwrap();
        let dst_md = fs::metadata(&dst).unwrap();

        // The trailing hole must survive: a data-extent-only copy would
        // truncate the file at the last byte of data.
        assert_eq!(dst_md.len(), APPARENT, "apparent size changed");
        // The point of the exercise.
        assert!(
            dst_md.blocks() * 512 <= src_actual + 2 * CHUNK as u64,
            "copy densified: src actual {src_actual}, dst actual {}",
            dst_md.blocks() * 512
        );
        assert!(
            written < APPARENT / 4,
            "wrote {written} bytes for {src_actual} bytes of data -- densified"
        );
        assert_eq!(
            fs::read(&src).unwrap(),
            fs::read(&dst).unwrap(),
            "contents differ"
        );
        // Private and writable, never the blob's 0444.
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&dst_md.permissions()) & 0o777,
            0o644
        );
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
    fn install_is_idempotent_and_leaves_no_staging_file() {
        let (d, s) = store();
        put_dummy(&s, "k", b"payload");
        let dest = d.path().join("c/img.ext4");
        for _ in 0..3 {
            s.install("k", &dest).unwrap();
        }
        assert_eq!(fs::read(&dest).unwrap(), b"payload");

        // Re-installing content that is already there used to leave a
        // `.part` link behind, because renaming one link of an inode onto
        // another link of the *same* inode is a POSIX no-op.
        assert!(
            !dest.with_extension("ext4.part").exists(),
            "staging file survived the install"
        );
        assert_eq!(
            fs::metadata(s.blob("k")).unwrap().nlink(),
            2,
            "expected exactly the blob and the installed name to link the inode"
        );
    }

    #[test]
    fn install_missing_key_errors() {
        let (d, s) = store();
        assert!(s.install("nope", &d.path().join("x.ext4")).is_err());
    }

    #[test]
    fn materialize_yields_an_independent_writable_file() {
        let (d, s) = store();
        put_dummy(&s, "k", b"payload");
        let dest = d.path().join("vm/rootfs.ext4");
        let m = s.materialize("k", &dest).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), b"payload");

        // Never a hardlink: that would hand out an alias of the shared 0444
        // blob, and writing through it would corrupt every other name at once.
        let blob = fs::metadata(s.blob("k")).unwrap();
        let out = fs::metadata(&dest).unwrap();
        assert_ne!(out.ino(), blob.ino(), "materialize returned a hardlink");
        assert_eq!(blob.nlink(), 1, "materialize altered the blob's link count");

        // Writable, unlike the blob.
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&out.permissions()) & 0o777,
            0o644
        );
        assert!(fs::OpenOptions::new().write(true).open(&dest).is_ok());

        // Whatever the filesystem did, the cost is reported.
        assert!(m.bytes_written() <= 4096, "unexpected cost: {m:?}");
    }

    #[test]
    fn materialize_is_repeatable_and_missing_key_errors() {
        let (d, s) = store();
        put_dummy(&s, "k", b"payload");
        let dest = d.path().join("vm/rootfs.ext4");
        s.materialize("k", &dest).unwrap();
        s.materialize("k", &dest).unwrap(); // overwrites cleanly
        assert_eq!(fs::read(&dest).unwrap(), b"payload");
        assert!(s.materialize("nope", &d.path().join("x.ext4")).is_err());
    }

    /// The whole point: materializing a sparse image must not densify it.
    #[test]
    #[cfg(target_os = "linux")]
    fn materialize_does_not_densify() {
        use std::io::{Seek, SeekFrom, Write};
        const APPARENT: u64 = 64 * 1024 * 1024;

        let (d, s) = store();
        let staging = s.staging_path("sp");
        {
            let mut f = fs::File::create(&staging).unwrap();
            f.set_len(APPARENT).unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(&vec![b'D'; 2 * 1024 * 1024]).unwrap();
            f.sync_all().unwrap();
        }
        s.put("sp", &staging, &["sha256:x".into()]).unwrap();

        let dest = d.path().join("vm/rootfs.ext4");
        let m = s.materialize("sp", &dest).unwrap();
        let out = fs::metadata(&dest).unwrap();

        assert_eq!(out.len(), APPARENT, "trailing hole lost");
        assert!(
            m.bytes_written() < APPARENT / 4,
            "densified: wrote {} of {APPARENT} apparent ({m:?})",
            m.bytes_written()
        );
        assert!(
            out.blocks() * 512 < APPARENT / 4,
            "densified on disk: {} bytes allocated",
            out.blocks() * 512
        );
    }

    #[test]
    fn release_reclaims_a_materialization_and_is_idempotent() {
        let (d, s) = store();
        put_dummy(&s, "k", b"payload");
        let dest = d.path().join("vm/rootfs.ext4");
        s.materialize("k", &dest).unwrap();
        assert!(dest.exists());

        s.release(&dest).unwrap();
        assert!(!dest.exists());
        // Releasing twice must not be an error: a caller reaping on shutdown
        // cannot know whether a crash already did it.
        s.release(&dest).unwrap();
        // The blob itself is untouched.
        assert!(s.get("k").is_some());
    }

    /// The trait must not change what `Store` does -- it only adds a seam.
    #[test]
    fn trait_impl_matches_inherent_behaviour() {
        let (d, s) = store();
        let be: &dyn StorageBackend = &s;

        let staging = be.staging_path("k");
        fs::write(&staging, b"payload").unwrap();
        be.put("k", &staging, &["sha256:x".into()]).unwrap();

        assert_eq!(be.get("k"), Store::get(&s, "k"));
        assert_eq!(be.entry("k").unwrap().size_bytes, 7);
        assert_eq!(be.root(), Store::root(&s));

        let dest = d.path().join("vm.ext4");
        be.materialize("k", &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"payload");
        be.release(&dest).unwrap();
        assert!(!dest.exists());
        assert_eq!(be.list().unwrap().len(), 1);
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

    /// A blob that cannot be removed must keep its metadata.
    ///
    /// Removing the meta regardless -- which is what the original code did, by
    /// discarding both `remove_file` results -- makes the blob vanish from
    /// `list()` while still occupying disk, so no later GC can ever find it.
    /// A read-only parent directory stands in here for the case that motivates
    /// it: ZFS returns EBUSY when destroying a dataset with dependent clones.
    #[test]
    #[cfg(target_os = "linux")]
    fn gc_keeps_metadata_when_the_blob_cannot_be_removed() {
        let (_d, s) = store();
        put_dummy(&s, "stuck", &[0u8; 8192]);

        // Make blobs/ read-only so unlink fails, mimicking EBUSY.
        let blobs = s.root().join("blobs");
        let mut perm = fs::metadata(&blobs).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o555);
        fs::set_permissions(&blobs, perm).unwrap();

        let r = s.gc(&GcPolicy {
            max_bytes: Some(0),
            ..Default::default()
        });

        // Restore before asserting, so a failure does not poison the tempdir.
        let mut perm = fs::metadata(&blobs).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        fs::set_permissions(&blobs, perm).unwrap();

        let r = r.unwrap();
        assert_eq!(r.kept_busy, vec!["stuck".to_string()], "report: {r:?}");
        assert!(r.removed.is_empty());
        assert!(
            s.entry("stuck").is_ok(),
            "metadata was deleted for a blob that still exists -- it is now invisible garbage"
        );
    }

    #[test]
    fn gc_measures_live_size_rather_than_trusting_recorded_bytes() {
        let (_d, s) = store();
        put_dummy(&s, "k", &[0u8; 8192]);
        // Corrupt the recorded size; GC should stat the blob instead.
        let mut e = s.entry("k").unwrap();
        e.actual_bytes = 999_999_999;
        s.write_meta(&e).unwrap();

        let r = s
            .gc(&GcPolicy {
                keep_secs: Some(0),
                dry_run: true,
                ..Default::default()
            })
            .unwrap();
        assert!(
            r.total_before < 1_000_000,
            "GC trusted the stale recorded size: {r:?}"
        );
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
