#!/bin/sh
# The default (posix) backend end to end. Needs docker and e2fsprogs; no root.
#
#   cargo build --release && ./examples/ext4.sh
set -eu
cd "$(dirname "$0")/.."
VMBUILD="${VMBUILD:-target/release/vmbuild}"
# Any Dockerfile works. Point these at a fastcar checkout to build the real
# thing: DOCKERFILE=../fastcar/deploy/image/Dockerfile CONTEXT=../fastcar
DOCKERFILE="${DOCKERFILE:-examples/minimal/Dockerfile}"
CONTEXT="${CONTEXT:-examples/minimal}"
WORK="${WORK:-$(mktemp -d)}"

# 1. Build. BuildKit does the Dockerfile; vmbuild turns the result into an
#    ext4 and files it under its diffID-chain key in the store
#    (~/.heyo/vmbuild by default; --store or $VMBUILD_STORE to move it).
"$VMBUILD" build -f "$DOCKERFILE" "$CONTEXT" -o "$WORK/rootfs.ext4"

# 2. Build again. Nothing changed, so the key is the same and the ext4 step is
#    skipped: the output is a hardlink to the stored blob, installed in ~0.2s.
"$VMBUILD" build -f "$DOCKERFILE" "$CONTEXT" -o "$WORK/rootfs.ext4"

# 3. The store can also install straight into heyvm's catalog by name.
#    (Commented out so this script leaves ~/.heyo/images alone.)
# "$VMBUILD" build -f "$DOCKERFILE" "$CONTEXT" -n fastcar

# 4. A VM must not boot the shared, read-only blob read-write. `materialize`
#    produces a private writable copy -- a reflink (FICLONE) on btrfs/XFS,
#    a sparse copy elsewhere. `doctor` says which you'll get before you pay.
KEY=$("$VMBUILD" build -f "$DOCKERFILE" "$CONTEXT" --json | sed -n 's/.*"key": *"\([^"]*\)".*/\1/p')
"$VMBUILD" doctor --dest "$WORK"
"$VMBUILD" materialize "$KEY" "$WORK/vm-1.ext4"
"$VMBUILD" verify "$WORK/vm-1.ext4"

# 5. When the VM is gone, release the copy, then let gc trim the store to a
#    byte budget (LRU; never touches a blob still linked into a catalog).
"$VMBUILD" release "$WORK/vm-1.ext4"
"$VMBUILD" cache ls
"$VMBUILD" cache gc --max-mb 20000

echo "done; scratch in $WORK"
