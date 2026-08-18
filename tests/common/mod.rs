//! Shared helpers: build tars in memory, then assert things about the ext4
//! image they produce.

#![allow(dead_code)]

use std::io::Cursor;
use std::path::Path;
use std::process::Command;
use vmbuild::ext4::{Ext4Options, SizePolicy, finish};

/// Is a tool on PATH? Tests that need e2fsprogs skip rather than fail when it
/// is absent, so the suite still runs somewhere without it.
pub fn have(tool: &str) -> bool {
    Command::new("which")
        .arg(tool)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A fully-initialized tar header. `tar::Header::new_gnu()` leaves uid/gid
/// blank, and tar-rs then errors with "numeric field was not a number" when
/// they are read back, so every field a reader touches must be set.
pub fn hdr(t: tar::EntryType, mode: u32, size: u64) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(t);
    h.set_mode(mode);
    h.set_size(size);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(1_600_000_000);
    h
}

/// Append an entry under a path `tar::Builder` would refuse to write (`..`,
/// absolute paths). The name is written straight into the header's name field
/// and the checksum recomputed, which is exactly what a hostile archive does.
pub fn append_raw_path(b: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]) {
    let mut h = hdr(tar::EntryType::Regular, 0o644, data.len() as u64);
    let name = &mut h.as_old_mut().name;
    name.fill(0);
    let bytes = path.as_bytes();
    name[..bytes.len()].copy_from_slice(bytes);
    h.set_cksum();
    b.append(&h, Cursor::new(data.to_vec())).unwrap();
}

/// One entry to place in a generated tar.
pub enum E {
    Dir(&'static str, u32),
    /// path, mode, contents
    File(&'static str, u32, &'static [u8]),
    /// path, target
    Sym(&'static str, &'static str),
    /// path, target (must appear earlier)
    Hard(&'static str, &'static str),
    /// path, mode, uid, gid
    OwnedFile(&'static str, u32, u32, u32),
}

/// Build a tar archive in memory from a list of entries.
pub fn make_tar(entries: &[E]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for e in entries {
        let mut h = tar::Header::new_gnu();
        h.set_mtime(1_600_000_000);
        h.set_uid(0);
        h.set_gid(0);
        match e {
            E::Dir(p, mode) => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(*mode);
                h.set_size(0);
                b.append_data(&mut h, p, std::io::empty()).unwrap();
            }
            E::File(p, mode, data) => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(*mode);
                h.set_size(data.len() as u64);
                b.append_data(&mut h, p, Cursor::new(*data)).unwrap();
            }
            E::OwnedFile(p, mode, uid, gid) => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(*mode);
                h.set_uid(*uid as u64);
                h.set_gid(*gid as u64);
                h.set_size(0);
                b.append_data(&mut h, p, std::io::empty()).unwrap();
            }
            E::Sym(p, target) => {
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_mode(0o777);
                h.set_size(0);
                b.append_link(&mut h, p, target).unwrap();
            }
            E::Hard(p, target) => {
                h.set_entry_type(tar::EntryType::Link);
                h.set_mode(0o644);
                h.set_size(0);
                b.append_link(&mut h, p, target).unwrap();
            }
        }
    }
    b.into_inner().unwrap()
}

pub fn opts() -> Ext4Options {
    Ext4Options {
        size: SizePolicy::Fixed(64 * 1024 * 1024),
        label: Some("test".into()),
        uuid: Some(uuid::Uuid::from_u128(
            0x1234_5678_9abc_def0_1234_5678_9abc_def0,
        )),
        journal: false, // most tests don't need e2fsprogs
        strict_special_files: false,
        epoch_secs: 1_600_000_000,
    }
}

/// Write `tar` to an image and return its path (inside `dir`).
pub fn build(dir: &Path, tar: &[u8], o: &Ext4Options) -> std::path::PathBuf {
    let out = dir.join("img.ext4");
    vmbuild::write_ext4_from_tar(Cursor::new(tar.to_vec()), &out, o).expect("write_ext4_from_tar");
    out
}

/// `e2fsck -fn` must exit 0. Deliberately stricter than heyvm's grow path,
/// which tolerates "errors were corrected".
pub fn assert_fsck_clean(image: &Path) {
    if !have("e2fsck") {
        eprintln!("skipping fsck assertion: e2fsck not on PATH");
        return;
    }
    match finish::fsck(image) {
        Ok(_) => {}
        Err(e) => panic!("e2fsck -fn was not clean for {}:\n{e}", image.display()),
    }
}

/// Read one inode field back out of the image via debugfs, e.g. ("Mode", ...).
pub fn debugfs_stat(image: &Path, path: &str) -> String {
    let out = Command::new("debugfs")
        .arg("-R")
        .arg(format!("stat {path}"))
        .arg(image)
        .output()
        .expect("run debugfs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}
