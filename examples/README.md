# Examples

| | |
|---|---|
| [`fastcar/`](fastcar/) | A production Dockerfile (~1.9 GB rootfs: node, postgres, chromium, a Rust-built tool) and the wrapper that drives vmbuild for it — reference, needs a fastcar checkout |
| [`minimal/`](minimal/) | A ~200 MB Debian + sshd rootfs the scripts below build by default |
| [`ext4.sh`](ext4.sh) | The default posix backend: build, rebuild from cache, hand a VM a private copy, reclaim |
| [`zfs.sh`](zfs.sh) | The same flow on the `zfs` backend, where a per-VM copy is a dataset clone |
| [`tar-to-ext4.sh`](tar-to-ext4.sh) | The ext4 writer alone, no Docker |

Every script is runnable as-is from the repo root after `cargo build --release`
(`fastcar/` aside; `zfs.sh` additionally needs `--features zfs`, root, and a pool).
