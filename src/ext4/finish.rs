//! Post-write steps that run e2fsprogs against a finished image.
//!
//! The one non-optional step is adding a journal. `arcbox_ext4` writes no
//! journal, but every image in heyvm's existing catalog has one (16 MiB), and
//! the guests need it: they boot `root=/dev/vda rw ... panic=1`, and
//! `kvm.rs:1179-1195` keeps the per-sandbox rootfs *persistent across
//! restarts*. `nginx.ext4` in the current catalog carries `needs_recovery`,
//! which is direct evidence that a VM died unclean and the journal is what
//! kept the filesystem intact. Shipping unjournaled images would be a
//! regression, not a simplification.

use crate::error::{Error, Result};
use std::path::Path;
use std::process::Command;

fn run(tool: &'static str, args: &[&str]) -> Result<String> {
    let out = Command::new(tool).args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::ToolMissing { tool }
        } else {
            Error::Io(e)
        }
    })?;
    if !out.status.success() {
        return Err(Error::ToolFailed {
            tool,
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Add an ext4 journal in place. Also doubles as a smoke test that e2fsprogs
/// accepts the image we just wrote.
pub fn add_journal(image: &Path) -> Result<()> {
    run("tune2fs", &["-j", &image.to_string_lossy()])?;
    Ok(())
}

// ext4 superblock lives at byte 1024; these are offsets within it.
const SB_BASE: u64 = 1024;
const SB_MTIME: u64 = 0x2C; // last mount time
const SB_WTIME: u64 = 0x30; // last write time
const SB_LASTCHECK: u64 = 0x40;

/// Erase the wall-clock timestamps that `tune2fs -j` (and `debugfs` itself)
/// stamp into an otherwise deterministic image.
///
/// The ext4 body vmbuild writes is already byte-reproducible. Adding a journal
/// is not: measured against two runs three seconds apart, exactly five bytes
/// differed -- the journal inode's atime/ctime/mtime/crtime, and the
/// superblock's `s_wtime`.
///
/// The journal inode (inode 8) is fixed via `debugfs`. `s_wtime` cannot be:
/// `debugfs` rewrites it when it closes the filesystem, so setting it *with*
/// `debugfs` is self-defeating. It is patched directly instead, at its fixed
/// offset, which is safe here only because these images carry no
/// `metadata_csum` -- a superblock checksum would be invalidated by a raw
/// write. That precondition is checked rather than assumed.
pub fn normalize_timestamps(image: &Path, epoch: u32) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    // Journal inode timestamps.
    let script = format!(
        "sif <8> atime @{epoch}\n\
         sif <8> ctime @{epoch}\n\
         sif <8> mtime @{epoch}\n\
         sif <8> crtime @{epoch}\n\
         quit\n"
    );
    let mut child = Command::new("debugfs")
        .arg("-w")
        .arg(image)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::ToolMissing { tool: "debugfs" }
            } else {
                Error::Io(e)
            }
        })?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(script.as_bytes())?;
    }
    child.wait()?;

    // Superblock timestamps, written directly -- see the doc comment.
    if features(image)?.iter().any(|f| f == "metadata_csum") {
        // Refuse to hand-patch a checksummed superblock. Reproducibility is a
        // convenience; a corrupt superblock is not.
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new().write(true).open(image)?;
    for off in [SB_MTIME, SB_WTIME, SB_LASTCHECK] {
        f.seek(SeekFrom::Start(SB_BASE + off))?;
        f.write_all(&epoch.to_le_bytes())?;
    }
    f.sync_all()?;
    Ok(())
}

/// `e2fsck -fn` -- read-only, exit 0 required.
///
/// Deliberately stricter than heyvm's `grow_ext4_image`, which tolerates exit
/// codes `& !3 == 0` (i.e. "errors were corrected"). Tolerating corrections
/// here would hide exactly the class of bug this check exists to catch.
pub fn fsck(image: &Path) -> Result<String> {
    let out = Command::new("e2fsck")
        .args(["-fn", &image.to_string_lossy()])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::ToolMissing { tool: "e2fsck" }
            } else {
                Error::Io(e)
            }
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.code() != Some(0) {
        return Err(Error::ToolFailed {
            tool: "e2fsck",
            status: out.status.to_string(),
            stderr: format!(
                "{}\n{}",
                stdout.trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .trim()
            .to_string(),
        });
    }
    Ok(stdout)
}

/// `dumpe2fs -h`, for feature-set assertions and `vmbuild inspect`.
pub fn dump_superblock(image: &Path) -> Result<String> {
    run("dumpe2fs", &["-h", &image.to_string_lossy()])
}

/// The feature flags parsed out of `dumpe2fs -h`.
pub fn features(image: &Path) -> Result<Vec<String>> {
    let text = dump_superblock(image)?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Filesystem features:") {
            return Ok(rest.split_whitespace().map(str::to_string).collect());
        }
    }
    Ok(Vec::new())
}
