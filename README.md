# vmbuild

Builds ext4 rootfs images for Firecracker/KVM microVMs from a Dockerfile —
fast, content-addressed, and without root.

```
vmbuild build -f Dockerfile . -o rootfs.ext4                     # Dockerfile -> bootable ext4, cached
vmbuild build -f deploy/image/Dockerfile . -n fastcar            # ...installed into heyvm's catalog
vmbuild materialize <key> /run/vm-1/rootfs.ext4                  # private writable copy for one VM (reflink where possible)
vmbuild --backend zfs --zfs-dataset tank/vmbuild --store /tank/vmbuild build -f Dockerfile .   # per-VM copies become zfs clones
vmbuild ext4 --from-tar rootfs.tar -o out.ext4                   # any tar -> ext4, no Docker
vmbuild verify out.ext4                                          # e2fsck -fn + feature set
```

Longer, runnable versions of each are in [`examples/`](examples/).

BuildKit does the `RUN`/`COPY` work; vmbuild owns the part that was previously
uncached: turning the built filesystem into an ext4 image, and remembering that
it did.

```
Dockerfile ──► buildx --load ──► diffID chain ──► cache key
                                                    ├─ hit  ──► hardlink (~0.2s)
                                                    └─ miss ──► buildx -o type=tar
                                                                 └─► ext4, written in-process
```

## Why

The pipeline this replaces went `docker build` → `docker create` →
`docker export` → `fakeroot tar -xf` → `fakeroot mke2fs -d` → `copy` → `copy`.
It wrote and read the whole rootfs about five times and cached nothing past
`docker build`, so an unchanged Dockerfile still paid the full cost.

Measured against that pipeline, same Dockerfile, same machine, docker layer
cache warm for both (minimum of 3 runs):

| | before | vmbuild | |
|---|---|---|---|
| first build | 7.36s | **1.53s** | 4.8× |
| rebuild, nothing changed | 6.00s | **0.21s** | **29×** |
| image on disk (actual) | 214 MB | **154 MB** | stays sparse |

The rebuild row is the one that matters day to day.

## How the cache key works

The key is the built image's **diffID chain**, read back from
`docker image inspect`. Not a hash of the Dockerfile and context.

That distinction is load-bearing:

- A comment-only Dockerfile edit produces the same diffIDs, so it still hits.
- Hashing the build context ourselves measured *slower* than asking BuildKit —
  0.18s to ask, versus 5.3s to hash a real 45k-file context — because
  BuildKit's context transfer is incremental and a naive walk is not.
- It cannot disagree with BuildKit about what changed, so it cannot serve the
  wrong image. There is no second opinion to get wrong.

The config digest is deliberately *not* used: it embeds `created`/`history`
timestamps that differ between two identical cached builds, so it would never
hit.

## Store

Built images live in a content-addressed store and are published into a
catalog by **hardlink**, so N names for one image cost one image:

```
<store>/blobs/<key>.ext4    mode 0444, shared
<store>/meta/<key>.json     size, diffIDs, last used
```

`vmbuild cache gc --max-mb N` evicts least-recently-used entries and never
touches a blob that is still linked into a catalog.

## Use

As a library — the ext4 writer is usable on its own, with no Docker involved:

```rust
use vmbuild::{Ext4Options, SizePolicy, write_ext4_from_tar};

let opts = Ext4Options { size: SizePolicy::FromTar { tar_bytes }, ..Default::default() };
let stats = write_ext4_from_tar(std::fs::File::open("rootfs.tar")?, "out.ext4".as_ref(), &opts)?;
```

Or as a CLI:

```
vmbuild build -f Dockerfile . -o rootfs.ext4     # build (cached)
vmbuild ext4 --from-tar rootfs.tar -o out.ext4   # tar -> ext4, no Docker
vmbuild cache ls | vmbuild cache gc --max-mb 20000
vmbuild verify out.ext4                          # e2fsck -fn + feature set
```

## Requirements

- **docker** with buildx (BuildKit). The `docker` driver is enough; no
  `docker-container` builder needed.
- **e2fsprogs** (`tune2fs`, `debugfs`) — only for the journal step, see below.

No root, no `fakeroot`, no loopback mount, no privileged container. Ownership,
permissions and setuid/setgid bits come from the tar headers and are written
straight into inodes, which is why no privileges are needed.

**Platforms.** Linux is the tested target. The crate compiles for macOS and the
ext4 writer is portable, but the journal step needs e2fsprogs, which macOS does
not ship (`brew install e2fsprogs`).

### About the journal

Images get an ext4 journal via `tune2fs -j`. This is not optional in practice:
guests boot `root=/dev/vda rw ... panic=1` and rootfs copies persist across
restarts, so an unclean shutdown leaves the filesystem needing recovery. In
testing, a Firecracker guest that force-rebooted while mounted read-write left
`needs_recovery` set, and `e2fsck -fp` replayed the journal cleanly — without
one, that is a corruption path.

## Reproducibility

Identical inputs produce byte-identical images. Per-entry timestamps come from
the tar, the filesystem UUID is derived from the cache key, and the wall-clock
stamps that `tune2fs` leaves behind are normalized afterwards. Honors
`SOURCE_DATE_EPOCH`.

## Vendored code

`src/arcbox_ext4/` is a fork of [`arcbox-ext4`](https://github.com/arcboxlabs/ext4-rs)
0.1.2 (Copyright © 2026 ArcBox Labs, MIT OR Apache-2.0), carried in-tree rather
than as a dependency because two of the patches add public API the upstream
release does not have, and two are silent correctness fixes. See
`src/arcbox_ext4/PATCHES.md` for all five and the reasoning; upstream's licences
are kept verbatim beside the code.

## Licence

MIT OR Apache-2.0, at your option — see [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
