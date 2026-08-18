//! The ext4 writer is the one place where a bug is silent and catastrophic:
//! a subtly wrong filesystem boots, runs, and corrupts later. These tests set
//! the bar accordingly -- every image any test produces must survive
//! `e2fsck -fn` with exit 0.

mod common;

use common::*;
use std::io::Cursor;
use vmbuild::ext4::{SizePolicy, finish};
use vmbuild::{Error, write_ext4_from_tar};

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn basic_tree_is_fsck_clean() {
    let d = tmp();
    let tar = make_tar(&[
        E::Dir("etc", 0o755),
        E::File("etc/hostname", 0o644, b"vmbuild\n"),
        E::Dir("usr", 0o755),
        E::Dir("usr/bin", 0o755),
        E::File("usr/bin/true", 0o755, b"\x7fELF-ish"),
        E::Sym("bin", "usr/bin"),
    ]);
    let img = build(d.path(), &tar, &opts());
    assert_fsck_clean(&img);
}

#[test]
fn empty_archive_still_produces_a_valid_filesystem() {
    let d = tmp();
    let img = build(d.path(), &make_tar(&[]), &opts());
    assert_fsck_clean(&img);
}

#[test]
fn ownership_and_special_mode_bits_survive() {
    if !have("debugfs") {
        return;
    }
    let d = tmp();
    let tar = make_tar(&[
        E::Dir("usr", 0o755),
        E::Dir("usr/bin", 0o755),
        // setuid root, setgid to a non-root group, and a plain non-root owner.
        E::OwnedFile("usr/bin/passwd", 0o4755, 0, 0),
        E::OwnedFile("usr/bin/chage", 0o2755, 0, 42),
        E::OwnedFile("home_file", 0o600, 1000, 1000),
    ]);
    let img = build(d.path(), &tar, &opts());
    assert_fsck_clean(&img);

    let s = debugfs_stat(&img, "/usr/bin/passwd");
    assert!(s.contains("04755"), "setuid bit lost:\n{s}");
    let s = debugfs_stat(&img, "/usr/bin/chage");
    assert!(s.contains("02755"), "setgid bit lost:\n{s}");
    assert!(s.contains("Group:    42"), "gid lost:\n{s}");
    let s = debugfs_stat(&img, "/home_file");
    assert!(
        s.contains("User:  1000") && s.contains("Group:  1000"),
        "non-root ownership lost -- this is the whole point of not needing fakeroot:\n{s}"
    );
}

#[test]
fn hardlinks_share_an_inode() {
    if !have("debugfs") {
        return;
    }
    let d = tmp();
    let tar = make_tar(&[
        E::Dir("usr", 0o755),
        E::File("usr/gunzip", 0o755, b"payload"),
        E::Hard("usr/uncompress", "usr/gunzip"),
    ]);
    let img = build(d.path(), &tar, &opts());
    assert_fsck_clean(&img);

    let ino = |p: &str| -> String {
        debugfs_stat(&img, p)
            .split_whitespace()
            .skip_while(|w| *w != "Inode:")
            .nth(1)
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(
        ino("/usr/gunzip"),
        ino("/usr/uncompress"),
        "hardlink was duplicated instead of sharing an inode"
    );
    assert!(
        debugfs_stat(&img, "/usr/gunzip").contains("Links: 2"),
        "link count not incremented"
    );
}

#[test]
fn mtimes_come_from_the_tar_not_the_clock() {
    if !have("debugfs") {
        return;
    }
    let d = tmp();
    // make_tar stamps every entry with 1_600_000_000 (2020-09-13).
    let tar = make_tar(&[E::File("f", 0o644, b"x")]);
    let img = build(d.path(), &tar, &opts());
    let s = debugfs_stat(&img, "/f");
    assert!(
        s.contains("0x5f5e1000") || s.contains("2020"),
        "mtime was not carried from the tar (old pipeline preserved it):\n{s}"
    );
}

#[test]
fn identical_input_produces_identical_bytes() {
    let d = tmp();
    let tar = make_tar(&[
        E::Dir("etc", 0o755),
        E::File("etc/a", 0o644, b"alpha"),
        E::Sym("etc/b", "a"),
    ]);
    let a = d.path().join("a.ext4");
    let b = d.path().join("b.ext4");
    let o = opts();
    write_ext4_from_tar(Cursor::new(tar.clone()), &a, &o).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100)); // cross a second
    write_ext4_from_tar(Cursor::new(tar.clone()), &b, &o).unwrap();
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "output is not byte-reproducible; a clock leaked into the image"
    );
}

#[test]
fn journal_is_present_and_image_stays_clean() {
    if !have("tune2fs") || !have("e2fsck") || !have("dumpe2fs") {
        return;
    }
    let d = tmp();
    let mut o = opts();
    o.journal = true;
    let tar = make_tar(&[E::File("f", 0o644, b"data")]);
    let img = build(d.path(), &tar, &o);

    let feats = finish::features(&img).unwrap();
    assert!(
        feats.iter().any(|f| f == "has_journal"),
        "no journal: guests boot rw with panic=1 and keep rootfs copies across \
         restarts, so an unjournaled image is a corruption generator. features={feats:?}"
    );
    assert_fsck_clean(&img);
}

#[test]
fn journalled_output_is_also_reproducible() {
    if !have("tune2fs") || !have("debugfs") {
        return;
    }
    let d = tmp();
    let mut o = opts();
    o.journal = true;
    let tar = make_tar(&[E::File("f", 0o644, b"data")]);
    let a = d.path().join("ja.ext4");
    let b = d.path().join("jb.ext4");
    write_ext4_from_tar(Cursor::new(tar.clone()), &a, &o).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write_ext4_from_tar(Cursor::new(tar.clone()), &b, &o).unwrap();
    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(&b).unwrap(),
        "tune2fs -j timestamps leaked; normalize_timestamps did not cover them"
    );
}

/// The gate that matters for KVM: heyvm grows every per-sandbox rootfs to
/// 20 GiB with `set_len` -> `e2fsck -fp` -> `resize2fs`, and treats any e2fsck
/// complaint as fatal. Our images carry no `resize_inode`, so this is the
/// check that it works anyway.
#[test]
fn survives_offline_resize_to_20_gib() {
    if !have("e2fsck") || !have("resize2fs") || !have("tune2fs") {
        return;
    }
    let d = tmp();
    let mut o = opts();
    o.journal = true;
    let tar = make_tar(&[E::Dir("etc", 0o755), E::File("etc/f", 0o644, b"data")]);
    let img = build(d.path(), &tar, &o);

    // Sparse grow -- costs no real disk.
    let f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    f.set_len(20 * 1024 * 1024 * 1024).unwrap();
    drop(f);

    let st = std::process::Command::new("e2fsck")
        .args(["-fp"])
        .arg(&img)
        .status()
        .unwrap();
    let code = st.code().unwrap_or(-1);
    assert!(
        code & !3 == 0,
        "e2fsck -fp returned {code}; heyvm's grow_ext4_image treats this as fatal"
    );

    let st = std::process::Command::new("resize2fs")
        .arg(&img)
        .status()
        .unwrap();
    assert!(st.success(), "resize2fs failed");
    assert_fsck_clean(&img);
}

// ---------------------------------------------------------------------------
// Adversarial input
// ---------------------------------------------------------------------------

#[test]
fn path_traversal_is_rejected() {
    let d = tmp();
    for bad in ["../escape", "a/../../escape"] {
        let mut b = tar::Builder::new(Vec::new());
        // tar::Builder refuses to *write* these, so forge the header directly.
        append_raw_path(&mut b, bad, b"bad");
        let tar = b.into_inner().unwrap();

        let out = d.path().join("esc.ext4");
        let r = write_ext4_from_tar(Cursor::new(tar), &out, &opts());
        assert!(
            matches!(r, Err(Error::PathEscape { .. })),
            "{bad:?} should be refused, got {r:?}"
        );
    }
}

#[test]
fn deep_nesting_and_wide_directories() {
    let d = tmp();
    let mut b = tar::Builder::new(Vec::new());
    let mut path = String::new();
    for i in 0..40 {
        path.push_str(&format!("d{i}/"));
        let mut h = hdr(tar::EntryType::Directory, 0o755, 0);
        b.append_data(&mut h, &path, std::io::empty()).unwrap();
    }
    // A wide directory -- also the regression canary for the O(n^2) lookup
    // that vmbuild patch 3 replaced with a hash index.
    for i in 0..3000 {
        let mut h = hdr(tar::EntryType::Regular, 0o644, 1);
        b.append_data(&mut h, format!("wide/f{i}"), Cursor::new(b"x".to_vec()))
            .unwrap();
    }
    let tar = b.into_inner().unwrap();
    let img = build(d.path(), &tar, &opts());
    assert_fsck_clean(&img);
}

#[test]
fn long_names_and_odd_but_legal_characters() {
    let d = tmp();
    let long = "n".repeat(200);
    let mut b = tar::Builder::new(Vec::new());
    for name in [long.as_str(), "sp ace", "dash-_.x", "üñïçø∂é"] {
        let mut h = hdr(tar::EntryType::Regular, 0o644, 2);
        b.append_data(&mut h, name, Cursor::new(b"ok".to_vec()))
            .unwrap();
    }
    let tar = b.into_inner().unwrap();
    let img = build(d.path(), &tar, &opts());
    assert_fsck_clean(&img);
}

#[test]
fn duplicate_entries_take_the_last_writer() {
    let d = tmp();
    let tar = make_tar(&[
        E::File("dup", 0o644, b"first"),
        E::File("dup", 0o644, b"second"),
    ]);
    let img = build(d.path(), &tar, &opts());
    assert_fsck_clean(&img);
}

#[test]
fn entry_whose_parent_appears_later() {
    // Exercises create()'s implicit mkdir -p path.
    let d = tmp();
    let tar = make_tar(&[
        E::File("a/b/c/file", 0o644, b"x"),
        E::Dir("a", 0o755),
        E::Dir("a/b", 0o755),
    ]);
    let img = build(d.path(), &tar, &opts());
    assert_fsck_clean(&img);
}

#[test]
fn files_at_extent_boundaries() {
    let d = tmp();
    let bs = 4096usize;
    let mut b = tar::Builder::new(Vec::new());
    for (i, n) in [0usize, 1, bs - 1, bs, bs + 1, bs * 12 + 7]
        .into_iter()
        .enumerate()
    {
        let data = vec![b'z'; n];
        let mut h = hdr(tar::EntryType::Regular, 0o644, n as u64);
        b.append_data(&mut h, format!("sz{i}"), Cursor::new(data))
            .unwrap();
    }
    let tar = b.into_inner().unwrap();
    let img = build(d.path(), &tar, &opts());
    assert_fsck_clean(&img);
}

#[test]
fn special_files_are_counted_not_silently_dropped() {
    let d = tmp();
    let mut b = tar::Builder::new(Vec::new());
    let mut h = hdr(tar::EntryType::Fifo, 0o644, 0);
    b.append_data(&mut h, "a_fifo", std::io::empty()).unwrap();
    let tar = b.into_inner().unwrap();

    let out = d.path().join("sp.ext4");
    let stats = write_ext4_from_tar(Cursor::new(tar.clone()), &out, &opts()).unwrap();
    assert_eq!(
        stats.skipped_special.len(),
        1,
        "special file was dropped without being reported"
    );

    // And --strict turns it into a hard error.
    let mut strict = opts();
    strict.strict_special_files = true;
    let out2 = d.path().join("sp2.ext4");
    assert!(matches!(
        write_ext4_from_tar(Cursor::new(tar), &out2, &strict),
        Err(Error::SpecialFiles { .. })
    ));
}

/// Sizes track the request instead of rounding up to a whole 128 MiB block
/// group (vmbuild patch 5).
///
/// The one adjustment left is the minimum-viable-tail rule: a partial final
/// group must still hold its own two bitmap blocks plus a full inode table
/// (512 blocks here), so a tail shorter than 514 blocks is grown to exactly
/// 514 -- about 2 MiB, not the 128 MiB a whole extra group would cost.
/// Getting this wrong produces a filesystem whose inode table runs past its
/// own end, which is what the fsck assertion below catches.
#[test]
fn size_tracks_the_request_with_a_partial_final_group() {
    const BLOCK: u64 = 4096;
    const BLOCKS_PER_GROUP: u64 = BLOCK * 8; // 32768
    const MIN_TAIL_BLOCKS: u64 = 512 + 2; // inode table + both bitmaps

    fn expected_bytes(requested_mib: u64) -> u64 {
        let mut blocks = requested_mib * 1024 * 1024 / BLOCK;
        let rem = blocks % BLOCKS_PER_GROUP;
        if rem != 0 && rem < MIN_TAIL_BLOCKS {
            blocks = blocks - rem + MIN_TAIL_BLOCKS;
        }
        blocks * BLOCK
    }

    // All comfortably larger than the payload, so nothing is grown to fit.
    for req in [
        128u64, 129, 130, 131, 135, 153, 200, 255, 256, 257, 258, 300,
    ] {
        let d = tmp();
        let tar = make_tar(&[E::File("f", 0o644, b"x")]);
        let mut o = opts();
        o.size = SizePolicy::Fixed(req * 1024 * 1024);
        let out = d.path().join("q.ext4");
        let stats = write_ext4_from_tar(Cursor::new(tar), &out, &o).unwrap();

        assert_eq!(
            stats.apparent_size,
            expected_bytes(req),
            "requested {req} MiB"
        );
        // Never more than one minimum-tail worth of slack -- in particular,
        // never rounded up to the next whole 128 MiB group.
        assert!(
            stats.apparent_size < req * 1024 * 1024 + MIN_TAIL_BLOCKS * BLOCK,
            "requested {req} MiB, got {} bytes -- looks like whole-group rounding",
            stats.apparent_size
        );
        // These files are attached to VMs as block devices; a size that is not
        // a whole number of blocks leaves a ragged tail.
        assert_eq!(
            stats.apparent_size % BLOCK,
            0,
            "image size is not block-aligned"
        );
        assert_fsck_clean(&out);
    }
}

/// Regression: a payload large enough to spill past the first block group.
///
/// Every other sizing test here uses a few KB of payload, which fits entirely
/// in group 0 -- and that blind spot hid a real bug. `close()` had a *second*
/// round-up, separate from the block-group alignment one: as soon as the
/// written data crossed a group boundary it forced the image up to that many
/// *whole* groups. A 212 MiB request holding 153 MiB of data became 256 MiB,
/// and no small-payload test could see it.
#[test]
fn payload_spanning_two_groups_is_not_rounded_up_to_whole_groups() {
    const MIB: u64 = 1024 * 1024;
    let d = tmp();

    // ~140 MiB of real data, so the filesystem must use part of group 1
    // (each group is 128 MiB).
    let mut b = tar::Builder::new(Vec::new());
    let chunk = vec![b'q'; 14 * MIB as usize];
    for i in 0..10 {
        let mut h = hdr(tar::EntryType::Regular, 0o644, chunk.len() as u64);
        b.append_data(&mut h, format!("big{i}"), Cursor::new(chunk.clone()))
            .unwrap();
    }
    let tar = b.into_inner().unwrap();

    let mut o = opts();
    o.size = SizePolicy::Fixed(200 * MIB); // between one and two whole groups
    let out = d.path().join("span.ext4");
    let stats = write_ext4_from_tar(Cursor::new(tar), &out, &o).unwrap();

    assert!(
        stats.apparent_size < 256 * MIB,
        "image was rounded up to whole block groups: {} bytes",
        stats.apparent_size
    );
    assert!(
        stats.apparent_size >= 140 * MIB,
        "image is too small to hold the payload: {} bytes",
        stats.apparent_size
    );
    assert_eq!(stats.apparent_size % 4096, 0, "not block-aligned");
    assert_fsck_clean(&out);
}
