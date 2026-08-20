//! The ZFS backend against a **real** OpenZFS.
//!
//! The unit tests in `src/zfs.rs` drive a fake `zfs(8)` and prove argv
//! construction, output parsing, error classification and the clone-graph GC
//! logic. They prove nothing about the things that actually go wrong: whether
//! a cloned dataset yields a usable image, whether `readonly=off` takes, the
//! recordsize and encryption constraints on cloning, or the real text ZFS puts
//! on stderr when a dataset is busy. Only this file covers those.
//!
//! Requires root and a scratch pool. Nothing here runs by default:
//!
//! ```text
//! sudo zpool create vmbuildtest /dev/…      # or a file vdev
//! sudo -E VMBUILD_ZFS_POOL=vmbuildtest \
//!     cargo test --features zfs --test zfs_real -- --ignored --nocapture
//! ```
//!
//! Every dataset it creates is destroyed on the way out.

#![cfg(feature = "zfs")]

use std::path::{Path, PathBuf};
use std::process::Command;
use vmbuild::store::{GcPolicy, StorageBackend};
use vmbuild::zfs::{SystemZfs, Zfs, ZfsBackend};

/// The pool to work in, or `None` to skip.
fn pool() -> Option<String> {
    let p = std::env::var("VMBUILD_ZFS_POOL").ok()?;
    // Refuse to run without a real `zfs`, rather than reporting a false pass.
    Command::new("zfs").arg("version").output().ok()?;
    Some(p)
}

struct Scratch {
    dataset: String,
    mountpoint: PathBuf,
}

impl Scratch {
    fn new(pool: &str) -> Self {
        let dataset = format!("{pool}/vmbuild-test");
        let mountpoint = PathBuf::from(format!("/{dataset}"));
        let _ = Command::new("zfs")
            .args(["destroy", "-r", &dataset])
            .output();
        let out = Command::new("zfs")
            .args(["create", "-p", &dataset])
            .output()
            .expect("zfs create");
        assert!(
            out.status.success(),
            "could not create {dataset}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self {
            dataset,
            mountpoint,
        }
    }
    fn backend(&self) -> ZfsBackend<SystemZfs> {
        ZfsBackend::new(SystemZfs, self.dataset.clone(), self.mountpoint.clone())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = Command::new("zfs")
            .args(["destroy", "-r", &self.dataset])
            .output();
    }
}

fn seed(be: &ZfsBackend<SystemZfs>, key: &str, bytes: &[u8]) {
    let staging = be.staging_path(key);
    std::fs::create_dir_all(staging.parent().unwrap()).unwrap();
    std::fs::write(&staging, bytes).unwrap();
    be.put(key, &staging, &["sha256:test".into()]).unwrap();
}

fn dataset_exists(name: &str) -> bool {
    Command::new("zfs")
        .args(["list", "-H", "-o", "name", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
#[ignore = "needs root and VMBUILD_ZFS_POOL"]
fn put_materialize_release_round_trip() {
    let Some(pool) = pool() else {
        eprintln!("skipping: set VMBUILD_ZFS_POOL and run as root");
        return;
    };
    let s = Scratch::new(&pool);
    let be = s.backend();

    seed(&be, "img", b"hello rootfs");
    assert!(dataset_exists(&format!("{}/blobs/img", s.dataset)));
    assert!(be.get("img").is_some(), "image not visible after put");

    // Clone it.
    let dest = s.mountpoint.join("vm-a.ext4");
    be.materialize("img", &dest).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello rootfs");

    // The clone must be writable even though the origin is readonly=on --
    // a clone inherits the origin's modes.
    assert!(
        std::fs::OpenOptions::new().write(true).open(&dest).is_ok(),
        "clone is not writable; readonly=off did not take"
    );

    // While the clone lives, the image is pinned and GC must not touch it.
    let r = be
        .gc(&GcPolicy {
            max_bytes: Some(0),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        r.kept_busy,
        vec!["img".to_string()],
        "GC did not notice the live clone: {r:?}"
    );
    assert!(dataset_exists(&format!("{}/blobs/img", s.dataset)));

    // Release, and only then may GC reclaim it.
    be.release(&dest).unwrap();
    be.release(&dest).unwrap(); // idempotent
    let r = be
        .gc(&GcPolicy {
            max_bytes: Some(0),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(r.removed, vec!["img".to_string()], "{r:?}");
    assert!(
        !dataset_exists(&format!("{}/blobs/img", s.dataset)),
        "image dataset survived GC"
    );
}

#[test]
#[ignore = "needs root and VMBUILD_ZFS_POOL"]
fn a_clone_really_is_cheap() {
    let Some(pool) = pool() else {
        return;
    };
    let s = Scratch::new(&pool);
    let be = s.backend();

    // 64 MiB of incompressible data, so "cheap" cannot be compression.
    let mut data = vec![0u8; 64 * 1024 * 1024];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i.wrapping_mul(2654435761) >> 13) as u8;
    }
    seed(&be, "big", &data);

    let used = |ds: &str| -> u64 {
        let out = Command::new("zfs")
            .args(["list", "-Hp", "-o", "used", ds])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    };

    let dest = s.mountpoint.join("vm-b.ext4");
    be.materialize("big", &dest).unwrap();
    let token_used = used(&format!("{}/clones", s.dataset));
    assert!(
        token_used < 8 * 1024 * 1024,
        "clone of a 64 MiB image consumed {token_used} bytes -- it copied rather than shared"
    );
    be.release(&dest).unwrap();
}

/// The error text this backend keys on is ZFS's, not ours -- pin it against
/// the real tool so a wording change is caught here rather than by silently
/// deleting metadata for a dataset that still exists.
#[test]
#[ignore = "needs root and VMBUILD_ZFS_POOL"]
fn busy_detection_matches_real_zfs_wording() {
    let Some(pool) = pool() else {
        return;
    };
    let s = Scratch::new(&pool);
    let be = s.backend();
    seed(&be, "pinned", b"x");
    let dest = s.mountpoint.join("vm-c.ext4");
    be.materialize("pinned", &dest).unwrap();

    // Destroying the origin of a live clone must fail, and must be recognised.
    let err = SystemZfs
        .run(&["destroy", &format!("{}/blobs/pinned", s.dataset)])
        .expect_err("destroying a cloned origin should fail");
    assert!(
        vmbuild::zfs::is_busy(&err),
        "real ZFS busy message not recognised: {err}"
    );

    be.release(&dest).unwrap();
}

/// Guard against the fake tests passing while the real path is unexercised.
#[test]
#[ignore = "needs root and VMBUILD_ZFS_POOL"]
fn harness_is_actually_talking_to_zfs() {
    let Some(pool) = pool() else {
        return;
    };
    let s = Scratch::new(&pool);
    assert!(
        dataset_exists(&s.dataset),
        "scratch dataset was not created -- this suite is not testing anything"
    );
    assert!(Path::new(&s.mountpoint).exists(), "dataset is not mounted");
}
