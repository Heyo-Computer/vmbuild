//! Differential test: the same tar through `mke2fs -d` (what heyvm does today,
//! under fakeroot) and through vmbuild, compared on *logical* content.
//!
//! Byte comparison would be meaningless -- the two differ deliberately in
//! feature flags (mke2fs sets resize_inode/dir_index/metadata_csum/64bit) and
//! in allocation layout. What must match is what the guest actually sees:
//! which paths exist, their type, mode, owner, size, contents, symlink
//! targets, and which paths share an inode.

mod common;

use common::*;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// One path's observable metadata.
#[derive(Debug, PartialEq, Eq)]
struct Meta {
    mode: String,
    uid: String,
    gid: String,
    size: String,
    kind: String,
}

/// Run one debugfs session that `stat`s every path, and parse the results.
fn stat_all(image: &Path, paths: &[String]) -> BTreeMap<String, Meta> {
    // debugfs has no `echo`, but it echoes each command back on its prompt
    // line ("debugfs:  stat /etc/hostname"), which serves as the delimiter.
    let mut script = String::new();
    for p in paths {
        script.push_str(&format!("stat {p}\n"));
    }
    script.push_str("quit\n");

    let mut child = Command::new("debugfs")
        .arg(image)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn debugfs");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();

    // A `stat` response spans several lines -- Mode and Type on the first,
    // User/Group/Size on a later one -- so accumulate the whole block per
    // path before parsing. (Scanning a single line silently yielded empty
    // uid/gid/size, which made the comparison below vacuous for those fields.)
    fn field(block: &str, key: &str) -> String {
        block
            .find(key)
            .map(|i| {
                block[i + key.len()..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .unwrap_or_default()
    }

    let mut map = BTreeMap::new();
    let mut cur: Option<String> = None;
    let mut block = String::new();
    let flush = |cur: &mut Option<String>, block: &mut String, map: &mut BTreeMap<_, _>| {
        if let Some(path) = cur.take()
            && block.contains("Mode:")
        {
            map.insert(
                path,
                Meta {
                    mode: field(block, "Mode:"),
                    uid: field(block, "User:"),
                    gid: field(block, "Group:"),
                    size: field(block, "Size:"),
                    kind: field(block, "Type:"),
                },
            );
        }
        block.clear();
    };

    for line in text.lines() {
        if let Some(rest) = line.split("debugfs:").nth(1) {
            let rest = rest.trim();
            flush(&mut cur, &mut block, &mut map);
            if let Some(p) = rest.strip_prefix("stat ") {
                cur = Some(p.trim().to_string());
            }
            continue;
        }
        if cur.is_some() {
            block.push_str(line);
            block.push('\n');
        }
    }
    flush(&mut cur, &mut block, &mut map);
    map
}

/// Recursively list every path in an image, using `rdump` into a scratch dir.
fn paths_of(image: &Path, scratch: &Path) -> Vec<String> {
    std::fs::create_dir_all(scratch).unwrap();
    let _ = Command::new("debugfs")
        .arg("-R")
        .arg(format!("rdump / {}", scratch.display()))
        .arg(image)
        .output();
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
            if rel == "lost+found" {
                continue; // ext4 bookkeeping, not tar content
            }
            out.push(format!("/{rel}"));
            if p.is_dir() && !p.is_symlink() {
                walk(base, &p, out);
            }
        }
    }
    walk(scratch, scratch, &mut out);
    out.sort();
    out
}

#[test]
fn matches_mke2fs_on_logical_content() {
    for tool in ["mke2fs", "debugfs", "fakeroot", "tar"] {
        if !have(tool) {
            eprintln!("skipping differential test: {tool} not on PATH");
            return;
        }
    }

    let d = tempfile::tempdir().unwrap();
    let tar_bytes = make_tar(&[
        E::Dir("etc", 0o755),
        E::File("etc/hostname", 0o644, b"vmbuild\n"),
        E::File("etc/empty", 0o600, b""),
        E::Dir("usr", 0o755),
        E::Dir("usr/bin", 0o755),
        E::OwnedFile("usr/bin/passwd", 0o4755, 0, 0),
        E::OwnedFile("usr/bin/chage", 0o2755, 0, 42),
        E::OwnedFile("usr/bin/owned", 0o600, 1000, 1000),
        E::File("usr/bin/gunzip", 0o755, b"payload-bytes-here"),
        E::Hard("usr/bin/uncompress", "usr/bin/gunzip"),
        E::Sym("bin", "usr/bin"),
        E::Dir("var", 0o755),
        E::Dir("var/local", 0o2775),
        E::File("big", 0o644, &[b'q'; 20000]),
    ]);

    let tar_path = d.path().join("in.tar");
    std::fs::write(&tar_path, &tar_bytes).unwrap();

    // --- Path A: what heyvm does today ---
    let staging = d.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let frstate = d.path().join("fr.state");
    let old = d.path().join("old.ext4");
    let ok = Command::new("fakeroot")
        .arg("-s")
        .arg(&frstate)
        .arg("--")
        .arg("tar")
        .arg("-xf")
        .arg(&tar_path)
        .arg("-C")
        .arg(&staging)
        .status()
        .unwrap()
        .success();
    assert!(ok, "fakeroot tar -xf failed");
    let ok = Command::new("fakeroot")
        .arg("-i")
        .arg(&frstate)
        .arg("--")
        .arg("mke2fs")
        .args(["-t", "ext4", "-d"])
        .arg(&staging)
        .args(["-L", "rootfs", "-q", "-F"])
        .arg(&old)
        .arg("64M")
        .status()
        .unwrap()
        .success();
    assert!(ok, "fakeroot mke2fs -d failed");

    // --- Path B: vmbuild ---
    let new = build(d.path(), &tar_bytes, &opts());

    // --- Compare ---
    let a_paths = paths_of(&old, &d.path().join("dump_old"));
    let b_paths = paths_of(&new, &d.path().join("dump_new"));
    assert_eq!(
        a_paths, b_paths,
        "path sets differ between mke2fs and vmbuild"
    );
    assert!(
        !a_paths.is_empty(),
        "rdump produced nothing; test is vacuous"
    );

    let a_meta = stat_all(&old, &a_paths);
    let b_meta = stat_all(&new, &b_paths);

    // Guard against a vacuous pass: if the debugfs parser silently returned
    // nothing, every comparison below would trivially agree. Pin one entry
    // whose expected values are known independently of both implementations.
    assert_eq!(a_meta.len(), a_paths.len(), "parser missed mke2fs paths");
    assert_eq!(b_meta.len(), b_paths.len(), "parser missed vmbuild paths");
    let passwd = b_meta
        .get("/usr/bin/passwd")
        .expect("/usr/bin/passwd absent from parsed metadata");
    assert_eq!(passwd.mode, "04755", "setuid not observed: {passwd:?}");
    let owned = b_meta
        .get("/usr/bin/owned")
        .expect("/usr/bin/owned absent from parsed metadata");
    assert_eq!(owned.uid, "1000", "non-root uid not observed: {owned:?}");
    assert_eq!(owned.gid, "1000", "non-root gid not observed: {owned:?}");

    let mut problems = Vec::new();
    for p in &a_paths {
        match (a_meta.get(p), b_meta.get(p)) {
            (Some(x), Some(y)) if x == y => {}
            (Some(x), Some(y)) => {
                problems.push(format!("{p}\n    mke2fs : {x:?}\n    vmbuild: {y:?}"))
            }
            (a, b) => problems.push(format!("{p}: missing stat (mke2fs={a:?} vmbuild={b:?})")),
        }
    }
    assert!(
        problems.is_empty(),
        "metadata differs from mke2fs -d for {} path(s):\n{}",
        problems.len(),
        problems.join("\n")
    );

    // Contents, compared from the rdump'd trees.
    for p in &a_paths {
        let ap = d.path().join("dump_old").join(p.trim_start_matches('/'));
        let bp = d.path().join("dump_new").join(p.trim_start_matches('/'));
        if ap.is_file() && !ap.is_symlink() {
            assert_eq!(
                std::fs::read(&ap).unwrap(),
                std::fs::read(&bp).unwrap(),
                "file contents differ for {p}"
            );
        }
        if ap.is_symlink() {
            assert_eq!(
                std::fs::read_link(&ap).unwrap(),
                std::fs::read_link(&bp).unwrap(),
                "symlink target differs for {p}"
            );
        }
    }

    assert_fsck_clean(&new);
}
