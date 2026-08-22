#!/bin/sh
# The ext4 writer on its own: any rootfs tar in, a bootable image out.
# No Docker, no root, no loop mount -- ownership and mode bits come from the
# tar headers and go straight into inodes. tune2fs/debugfs add the journal.
#
#   cargo build --release && ./examples/tar-to-ext4.sh
set -eu
cd "$(dirname "$0")/.."
VMBUILD="${VMBUILD:-target/release/vmbuild}"
WORK="${WORK:-$(mktemp -d)}"

# Any tar works; this one comes from an image so it has a real / layout.
docker create --name vmbuild-example alpine:3 >/dev/null
docker export vmbuild-example > "$WORK/rootfs.tar"
docker rm vmbuild-example >/dev/null

# Size defaults to tar*1.2 + 64MB. --strict refuses device nodes/FIFOs/sockets
# instead of skipping them. Byte-identical for identical input; honours
# SOURCE_DATE_EPOCH.
"$VMBUILD" ext4 --from-tar "$WORK/rootfs.tar" -o "$WORK/alpine.ext4" --label rootfs
"$VMBUILD" verify "$WORK/alpine.ext4"

# Or stream it, no intermediate file. The image size has to be fixed before
# the first block is written, and a pipe has no length, so stdin needs
# --size-mb (here, the same 128 MiB the auto-size chose above).
cat "$WORK/rootfs.tar" | "$VMBUILD" ext4 --from-tar - --size-mb 128 -o "$WORK/alpine-streamed.ext4"

# Same tar, same size, same bytes out. (A second `docker export` would not
# match: Docker writes the container ID into /etc/hostname.)
cmp "$WORK/alpine.ext4" "$WORK/alpine-streamed.ext4" && echo "byte-identical"

echo "done; images in $WORK"
