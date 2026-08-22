#!/bin/sh
# The zfs backend: one dataset per image, one clone per VM. A materialization
# is a `zfs clone`, so a per-VM rootfs costs zero bytes and zero copy time
# regardless of image size, and `release` is `zfs destroy`.
#
# Experimental, and root-only: Linux cannot delegate the `mount` permission
# that zfs create/clone/destroy all need. Never selected automatically --
# a machine that happens to sit on a zpool does not change behaviour.
#
#   cargo build --release --features zfs
#   sudo zpool create tank /dev/...             # or a file vdev for a trial:
#   # truncate -s 20G /var/tmp/tank.img && sudo zpool create tank /var/tmp/tank.img
#   sudo -E ./examples/zfs.sh
set -eu
cd "$(dirname "$0")/.."
VMBUILD="${VMBUILD:-target/release/vmbuild}"
# Any Dockerfile works. Point these at a fastcar checkout to build the real
# thing: DOCKERFILE=../fastcar/deploy/image/Dockerfile CONTEXT=../fastcar
DOCKERFILE="${DOCKERFILE:-examples/minimal/Dockerfile}"
CONTEXT="${CONTEXT:-examples/minimal}"
POOL="${POOL:-tank}"
DATASET="$POOL/vmbuild"
WORK="${WORK:-/$POOL/vms}"

# The backend needs a parent dataset to create image datasets under, and its
# mountpoint as the store root. Every vmbuild command takes the same three
# flags, so pin them once.
zfs list "$DATASET" >/dev/null 2>&1 || zfs create -p "$DATASET"
set -- --backend zfs --zfs-dataset "$DATASET" --store "/$DATASET"
mkdir -p "$WORK"

# 1. Build. Identical to the posix path up to the last step, where the ext4
#    lands in <dataset>/blobs/<key> and is snapshotted @ready.
KEY=$("$VMBUILD" "$@" build -f "$DOCKERFILE" "$CONTEXT" --json \
      | sed -n 's/.*"key": *"\([^"]*\)".*/\1/p')
echo "image key: $KEY"

# 2. Hand three VMs their own rootfs. Each is a clone of the @ready snapshot:
#    instant, shares every block with its origin, and writable.
for vm in 1 2 3; do
    "$VMBUILD" "$@" materialize "$KEY" "$WORK/vm-$vm.ext4"
done
zfs list -t all -r "$DATASET"

# 3. Release is not optional here. A clone pins its origin snapshot, so an
#    unreleased VM keeps the image alive through any number of `cache gc`s.
for vm in 1 2 3; do
    "$VMBUILD" "$@" release "$WORK/vm-$vm.ext4"
done
"$VMBUILD" "$@" cache ls
"$VMBUILD" "$@" cache gc --max-mb 20000
