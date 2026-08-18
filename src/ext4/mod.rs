//! Stream a rootfs tar directly into an ext4 image.
//!
//! One read of the tar, one write of the image, no staging directory, no
//! `fakeroot`, no `mke2fs`, no root. Ownership and permissions come from the
//! tar headers and are written straight into inodes, which is why this needs
//! no privileges at all -- the thing `fakeroot` was being used to fake.
//!
//! The input is expected to be a *flattened* rootfs (BuildKit's
//! `--output type=tar`), not a stack of OCI layers: no `.wh.` whiteouts, no
//! opaque-directory markers, entries in dependency order. BuildKit has already
//! done that merge, which is why there is no whiteout tracker here.

pub mod finish;
pub mod sizing;

use crate::error::{Error, Result};
use crate::arcbox_ext4::{FileTimestamps, FormatOptions, Formatter};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub use sizing::SizePolicy;

// File-type bits (`S_IF*`). arcbox's `create()` takes a mode with the type
// bits included.
const S_IFREG: u16 = 0x8000;
const S_IFDIR: u16 = 0x4000;
const S_IFLNK: u16 = 0xA000;

#[derive(Debug, Clone)]
pub struct Ext4Options {
    pub size: SizePolicy,
    pub label: Option<String>,
    /// Fixing this makes the image byte-reproducible; `None` picks a random
    /// v4 UUID and the output differs between otherwise-identical runs.
    pub uuid: Option<Uuid>,
    /// Add a journal with `tune2fs -j`. Effectively always on -- see
    /// `finish::add_journal` for why.
    pub journal: bool,
    /// Fail rather than warn when the tar contains entries that cannot be
    /// represented (device nodes, FIFOs, sockets).
    pub strict_special_files: bool,
    /// Timestamp for the inodes ext4 requires but the tar does not describe:
    /// the root directory and `/lost+found`. Together with a fixed `uuid` and
    /// the per-entry mtimes taken from the tar, this makes the image
    /// byte-reproducible. Honors `SOURCE_DATE_EPOCH` by convention.
    pub epoch_secs: u32,
}

impl Default for Ext4Options {
    fn default() -> Self {
        Self {
            size: SizePolicy::FromTar { tar_bytes: 0 },
            label: Some("rootfs".to_string()),
            uuid: None,
            journal: true,
            strict_special_files: false,
            epoch_secs: default_epoch(),
        }
    }
}

/// `SOURCE_DATE_EPOCH` if set and parseable, else a fixed constant.
///
/// Deliberately *not* the wall clock: a default of `now` would make output
/// non-reproducible by accident, which is the failure mode this exists to
/// prevent. The constant is 2020-01-01T00:00:00Z.
pub fn default_epoch() -> u32 {
    const FALLBACK: u32 = 1_577_836_800;
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(FALLBACK)
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Ext4Stats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub hardlinks: u64,
    pub bytes_from_tar: u64,
    /// Entries we could not represent. Surfaced rather than silently dropped:
    /// upstream's `unpack_tar` discards these with a bare `_ => continue`, and
    /// the day it matters you want to know.
    pub skipped_special: Vec<PathBuf>,
    pub apparent_size: u64,
    pub actual_size: u64,
}

/// Normalize a tar entry path to an absolute image path, rejecting anything
/// that escapes the root.
fn normalize(raw: &Path) -> Result<Option<String>> {
    let mut out = String::new();
    for c in raw.components() {
        match c {
            Component::Normal(part) => {
                let s = part.to_str().ok_or_else(|| Error::BadPath {
                    path: raw.to_path_buf(),
                })?;
                out.push('/');
                out.push_str(s);
            }
            // `./foo` and a leading `/` are both fine; drop the prefix.
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(Error::PathEscape {
                    path: raw.to_path_buf(),
                });
            }
        }
    }
    // The archive's own "./" entry maps to the root, which already exists.
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
}

/// A hardlink, deferred until the end of the archive so its target is present.
struct PendingLink {
    link: String,
    target: String,
}

/// All four inode timestamps set to the same second.
fn fixed_ts(secs: u32) -> FileTimestamps {
    FileTimestamps {
        access_lo: secs,
        access_hi: 0,
        modification_lo: secs,
        modification_hi: 0,
        creation_lo: secs,
        creation_hi: 0,
        now_lo: secs,
        now_hi: 0,
    }
}

pub fn write_ext4_from_tar<R: Read>(tar: R, out: &Path, opts: &Ext4Options) -> Result<Ext4Stats> {
    let size = opts.size.resolve();

    let mut fopts = FormatOptions::new(size);
    fopts.uuid = opts.uuid;
    fopts.label = opts.label.clone();
    fopts.epoch = Some((opts.epoch_secs, 0));
    // Do not round the image up to a whole 128 MiB block group.
    fopts.allow_partial_final_group = true;
    let mut fmt = Formatter::with_options(out, fopts)?;

    let mut stats = Ext4Stats::default();
    let mut pending: Vec<PendingLink> = Vec::new();

    let mut archive = tar::Archive::new(tar);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let header = entry.header();
        let etype = header.entry_type();

        // PAX/GNU metadata records carry no filesystem entry of their own.
        if etype.is_pax_global_extensions()
            || etype.is_pax_local_extensions()
            || etype.is_gnu_longname()
            || etype.is_gnu_longlink()
        {
            continue;
        }

        let raw = entry.path()?.into_owned();
        let Some(path) = normalize(&raw)? else {
            continue;
        };

        let perm = (header.mode()? & 0o7777) as u16;
        let uid = header.uid()? as u32;
        let gid = header.gid()? as u32;
        let size_hint = header.size()?;
        // Carry the archive's mtime onto the inode. Without this every entry
        // is stamped with the wall clock, which both loses fidelity against
        // the old `tar -xf` + `mke2fs -d` pipeline (which preserved mtimes)
        // and makes the image non-reproducible.
        let ts = Some(fixed_ts(header.mtime()? as u32));

        if etype.is_dir() {
            fmt.create(
                &path,
                S_IFDIR | perm,
                None,
                ts,
                None,
                Some(uid),
                Some(gid),
                None,
            )?;
            stats.dirs += 1;
        } else if etype.is_symlink() {
            let target = header
                .link_name()?
                .ok_or_else(|| Error::BadPath { path: raw.clone() })?
                .to_string_lossy()
                .into_owned();
            fmt.create(
                &path,
                S_IFLNK | perm,
                Some(&target),
                ts,
                None,
                Some(uid),
                Some(gid),
                None,
            )?;
            stats.symlinks += 1;
        } else if etype.is_hard_link() {
            let target_raw = entry
                .link_name()?
                .ok_or_else(|| Error::BadPath { path: raw.clone() })?
                .into_owned();
            if let Some(target) = normalize(&target_raw)? {
                pending.push(PendingLink { link: path, target });
            }
        } else if etype.is_file() {
            fmt.create(
                &path,
                S_IFREG | perm,
                None,
                ts,
                Some(&mut entry as &mut dyn Read),
                Some(uid),
                Some(gid),
                None,
            )?;
            stats.files += 1;
            stats.bytes_from_tar += size_hint;
        } else {
            // Character/block devices, FIFOs, sockets. Not representable by
            // the formatter. M0 measured zero of these across a real debian
            // rootfs (4,225 entries), because /dev is a runtime tmpfs and is
            // never part of a layer -- but count them rather than drop them.
            stats.skipped_special.push(raw);
        }
    }

    // Hardlinks last: the target inode must already exist.
    for PendingLink { link, target } in pending {
        fmt.link(&link, &target)?;
        stats.hardlinks += 1;
    }

    fmt.close()?;

    if !stats.skipped_special.is_empty() && opts.strict_special_files {
        return Err(Error::SpecialFiles {
            count: stats.skipped_special.len(),
            sample: stats.skipped_special.iter().take(5).cloned().collect(),
        });
    }

    if opts.journal {
        finish::add_journal(out)?;
        // tune2fs stamps the wall clock into the journal inode and the
        // superblock; undo that so identical inputs still hash identically.
        finish::normalize_timestamps(out, opts.epoch_secs)?;
    }

    let meta = std::fs::metadata(out)?;
    stats.apparent_size = meta.len();
    stats.actual_size = {
        use std::os::unix::fs::MetadataExt;
        meta.blocks() * 512
    };

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_prefixes() {
        assert_eq!(
            normalize(Path::new("usr/bin")).unwrap().unwrap(),
            "/usr/bin"
        );
        assert_eq!(
            normalize(Path::new("/usr/bin")).unwrap().unwrap(),
            "/usr/bin"
        );
        assert_eq!(
            normalize(Path::new("./usr/bin")).unwrap().unwrap(),
            "/usr/bin"
        );
    }

    #[test]
    fn normalize_maps_archive_root_to_none() {
        assert!(normalize(Path::new("./")).unwrap().is_none());
        assert!(normalize(Path::new("/")).unwrap().is_none());
    }

    #[test]
    fn normalize_rejects_escapes() {
        for bad in ["../etc/passwd", "usr/../../etc", "a/b/../../.."] {
            assert!(
                matches!(normalize(Path::new(bad)), Err(Error::PathEscape { .. })),
                "{bad} should be rejected"
            );
        }
    }
}
