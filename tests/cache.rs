//! The content-addressed cache, end to end.
//!
//! The properties that matter are not "it is fast" but:
//!   * a rebuild with unchanged inputs reuses the stored image,
//!   * a change that does not alter the rootfs still hits (this is why the key
//!     is the diffID chain and not a hash of the build inputs),
//!   * a change that does alter the rootfs misses,
//!   * installing costs no extra disk.

mod common;

use common::have;
use std::path::Path;
use std::process::Command;
use vmbuild::build::{BuildRequest, CacheHit, build};
use vmbuild::buildkit::{BuildSpec, ContextSource, DockerfileSource};
use vmbuild::ext4::Ext4Options;
use vmbuild::store::Store;

fn docker_ready() -> bool {
    have("docker")
        && Command::new("docker")
            .args(["info"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

fn req(dir: &Path, out: &Path) -> BuildRequest {
    BuildRequest {
        dockerfile: DockerfileSource::Path(dir.join("Dockerfile")),
        context: ContextSource::Dir(dir.to_path_buf()),
        spec: BuildSpec::default(),
        ext4: Ext4Options::default(),
        install_to: Some(out.to_path_buf()),
        refresh: false,
    }
}

#[test]
#[ignore = "needs a working docker daemon"]
fn cache_hits_misses_and_dedups() {
    if !docker_ready() {
        eprintln!("skipping: docker unavailable");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let store = Store::open(&d.path().join("store")).unwrap();
    std::fs::write(
        d.path().join("Dockerfile"),
        "FROM alpine:3.21\nRUN echo one > /marker\n",
    )
    .unwrap();

    // Cold.
    let a = build(&req(d.path(), &d.path().join("a.ext4")), &store).unwrap();
    assert_eq!(
        a.cache,
        CacheHit::Miss,
        "first build must populate the cache"
    );
    assert!(a.stats.is_some());

    // Warm: identical inputs.
    let b = build(&req(d.path(), &d.path().join("b.ext4")), &store).unwrap();
    assert_eq!(b.cache, CacheHit::Hit);
    assert_eq!(a.key, b.key);
    assert!(b.stats.is_none(), "a hit must not rebuild the ext4");

    // A comment changes the Dockerfile but not the rootfs, so the diffIDs are
    // unchanged and this must still hit. Hashing the build inputs instead
    // would miss here and rebuild for nothing.
    std::fs::write(
        d.path().join("Dockerfile"),
        "FROM alpine:3.21\n# a comment\nRUN echo one > /marker\n",
    )
    .unwrap();
    let c = build(&req(d.path(), &d.path().join("c.ext4")), &store).unwrap();
    assert_eq!(c.cache, CacheHit::Hit, "comment-only edit should still hit");
    assert_eq!(a.key, c.key);

    // A real content change must miss and produce a different key.
    std::fs::write(
        d.path().join("Dockerfile"),
        "FROM alpine:3.21\nRUN echo two > /marker\n",
    )
    .unwrap();
    let e = build(&req(d.path(), &d.path().join("e.ext4")), &store).unwrap();
    assert_eq!(e.cache, CacheHit::Miss, "content change must rebuild");
    assert_ne!(a.key, e.key);

    // Installing is a hardlink, so three catalog names for one image cost one
    // image's worth of disk.
    use std::os::unix::fs::MetadataExt;
    let ino = |p: &Path| std::fs::metadata(p).unwrap().ino();
    assert_eq!(ino(&d.path().join("a.ext4")), ino(&d.path().join("b.ext4")));
    assert_eq!(ino(&d.path().join("a.ext4")), ino(&d.path().join("c.ext4")));
    assert_ne!(ino(&d.path().join("a.ext4")), ino(&d.path().join("e.ext4")));

    // --refresh rebuilds even on a hit, and lands on the same key.
    let mut r = req(d.path(), &d.path().join("f.ext4"));
    r.refresh = true;
    let f = build(&r, &store).unwrap();
    assert_eq!(f.cache, CacheHit::Miss);
    assert_eq!(f.key, e.key);
}
