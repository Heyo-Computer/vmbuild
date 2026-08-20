//! What will this machine actually do with a copy?
//!
//! Copy-on-write support cannot be predicted from the filesystem name. It is a
//! property of the **(source, destination) pair**: `FICLONE` returns EXDEV
//! across mounts, and OpenZFS refuses cross-dataset clones when `recordsize` or
//! the encryption key differ. Worse, OpenZFS gates cloning on a tunable whose
//! default flipped between releases -- 2.2.0 shipped it on, 2.2.1 through
//! 2.2.10 turned it off after a corruption bug, 2.3.0 turned it back on -- and
//! the 2.2 manual documents the default *incorrectly*. So this probes rather
//! than infers, and reports the exact pair it tested.
//!
//! This is advisory only. [`crate::store::Store::materialize`] never consults
//! it: it always attempts `FICLONE` and reports what actually happened. A
//! diagnostic that changed behaviour would be worse than none.

use crate::error::Result;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub source_dir: PathBuf,
    pub dest_dir: PathBuf,
    pub source_fs: String,
    pub dest_fs: String,
    /// Whether `FICLONE` actually worked for this pair, verified by reading the
    /// clone back. `None` means the probe could not run.
    pub reflink: Option<bool>,
    pub reflink_detail: String,
    /// Present only when the ZFS module is loaded.
    pub zfs: Option<Zfs>,
    pub verdict: String,
}

#[derive(Debug, serde::Serialize)]
pub struct Zfs {
    pub version: String,
    /// `None` when `/sys/module/zfs/parameters/zfs_bclone_enabled` is
    /// unreadable -- reported as unknown rather than guessed.
    pub bclone_enabled: Option<bool>,
    pub note: String,
}

/// Filesystem name for `path`, from its statfs magic.
fn fs_name(path: &Path) -> String {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
            return "unknown".into();
        };
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
            return "unknown".into();
        }
        match st.f_type as i64 {
            0xEF53 => "ext2/3/4",
            0x9123683E => "btrfs",
            0x58465342 => "xfs",
            0x2FC12FC1 => "zfs",
            0x01021994 => "tmpfs",
            0x794C7630 => "overlayfs",
            0x6969 => "nfs",
            0x65735546 => "fuse",
            other => return format!("unknown (0x{other:x})"),
        }
        .to_string()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        "unknown".into()
    }
}

/// Attempt a real clone between two directories and verify the result.
///
/// Uses unnamed temporary files, so nothing is left behind even if the process
/// is killed mid-probe. Writes a full block of real data first: cloning an
/// empty or hole-only file can succeed trivially and would report a false
/// positive.
#[cfg(target_os = "linux")]
fn probe_reflink(src_dir: &Path, dest_dir: &Path) -> (Option<bool>, String) {
    use std::os::fd::AsRawFd;
    const FICLONE: libc::c_ulong = 0x4004_9409;
    const PAYLOAD: usize = 1 << 20;

    let mut src = match tempfile::tempfile_in(src_dir) {
        Ok(f) => f,
        Err(e) => {
            return (
                None,
                format!("cannot create a temp file in source dir: {e}"),
            );
        }
    };
    let data = vec![0xA5u8; PAYLOAD];
    if let Err(e) = src.write_all(&data).and_then(|()| src.sync_all()) {
        return (None, format!("cannot write the probe payload: {e}"));
    }
    let dst = match tempfile::tempfile_in(dest_dir) {
        Ok(f) => f,
        Err(e) => return (None, format!("cannot create a temp file in dest dir: {e}")),
    };

    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd()) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        return (Some(false), format!("FICLONE refused: {e}"));
    }

    // Verify: a clone that reports success but yields different bytes is worse
    // than one that fails.
    let mut back = Vec::with_capacity(PAYLOAD);
    let mut dst = dst;
    if dst
        .seek(SeekFrom::Start(0))
        .and_then(|_| dst.read_to_end(&mut back))
        .is_err()
    {
        return (
            Some(false),
            "clone succeeded but could not be read back".into(),
        );
    }
    if back != data {
        return (
            Some(false),
            "clone succeeded but the data read back differs -- do not trust it".into(),
        );
    }
    (Some(true), "FICLONE verified by read-back".into())
}

#[cfg(not(target_os = "linux"))]
fn probe_reflink(_src: &Path, _dest: &Path) -> (Option<bool>, String) {
    (None, "reflink probing is implemented for Linux only".into())
}

fn zfs_info() -> Option<Zfs> {
    let version = fs::read_to_string("/sys/module/zfs/version").ok()?;
    let version = version.trim().to_string();
    // World-readable when present. /dev/zfs is often 0600 root, so we
    // deliberately do not ask the zfs(8) tooling anything here.
    let bclone_enabled = fs::read_to_string("/sys/module/zfs/parameters/zfs_bclone_enabled")
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .map(|v| v != 0);
    let note = match bclone_enabled {
        Some(true) => "block cloning enabled; the pool also needs feature@block_cloning".into(),
        Some(false) => "block cloning disabled (zfs_bclone_enabled=0) -- the default on \
                        OpenZFS 2.2.1 through 2.2.10; copies will not be shared"
            .into(),
        None => "zfs_bclone_enabled unreadable; treating as unknown".into(),
    };
    Some(Zfs {
        version,
        bclone_enabled,
        note,
    })
}

/// Probe what this machine will do, without creating or touching a store.
pub fn run(source_dir: &Path, dest_dir: &Path) -> Result<Report> {
    let (reflink, reflink_detail) = probe_reflink(source_dir, dest_dir);
    let zfs = zfs_info();

    let verdict = match reflink {
        Some(true) => "copies between these directories share blocks; a per-VM \
                       rootfs costs almost nothing"
            .to_string(),
        Some(false) => "no block sharing between these directories; vmbuild will \
                        fall back to a hole-preserving copy, which still writes \
                        only the image's real data rather than its apparent size"
            .to_string(),
        None => "could not determine block sharing; vmbuild will attempt it anyway \
                 and fall back to a hole-preserving copy"
            .to_string(),
    };

    Ok(Report {
        source_fs: fs_name(source_dir),
        dest_fs: fs_name(dest_dir),
        source_dir: source_dir.to_path_buf(),
        dest_dir: dest_dir.to_path_buf(),
        reflink,
        reflink_detail,
        zfs,
        verdict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_leaves_nothing_behind() {
        let d = tempfile::tempdir().unwrap();
        let before: Vec<_> = fs::read_dir(d.path()).unwrap().flatten().collect();
        let _ = run(d.path(), d.path()).unwrap();
        let after: Vec<_> = fs::read_dir(d.path()).unwrap().flatten().collect();
        assert_eq!(before.len(), after.len(), "probe left files behind");
        assert!(after.is_empty());
    }

    #[test]
    fn reports_the_pair_it_tested() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let r = run(a.path(), b.path()).unwrap();
        assert_eq!(r.source_dir, a.path());
        assert_eq!(r.dest_dir, b.path());
        assert!(!r.verdict.is_empty());
    }

    /// On an ext4 host FICLONE is genuinely unsupported, so the probe must say
    /// so rather than optimistically reporting support.
    #[test]
    #[cfg(target_os = "linux")]
    fn probe_is_honest_about_this_host() {
        let d = tempfile::tempdir().unwrap();
        let r = run(d.path(), d.path()).unwrap();
        if r.source_fs.starts_with("ext2") {
            assert_eq!(
                r.reflink,
                Some(false),
                "ext4 has no reflink; probe said {:?} ({})",
                r.reflink,
                r.reflink_detail
            );
        }
    }
}
