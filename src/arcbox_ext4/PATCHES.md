# Local patches to `arcbox-ext4`

Forked from crates.io `arcbox-ext4` **0.1.2** (`https://github.com/arcboxlabs/ext4-rs`),
MIT OR Apache-2.0.

Why a fork rather than a dependency: upstream is a single-vendor `0.1.x` whose published
crate sets `exclude = ["/tests"]`, so it ships **no tests at all**. Several fixes below are
required before the output survives `e2fsck`, and a crates.io dependency would block every
one of them on an unknown maintainer. Patches are kept small and upstreamable so rebasing
onto a future 0.1.3 stays cheap.

Every change is marked in-source with a `vmbuild patch N:` comment.

---

## Patch 1 — `creator_os` was FreeBSD

`src/formatter.rs`, superblock init.

Upstream wrote `sb.creator_os = 3` (FreeBSD), commented "matches Apple's implementation".
These images are consumed by Linux guests and by host-side e2fsprogs
(`e2fsck`/`resize2fs`/`tune2fs`). `creator_os` changes how several inode fields are
interpreted — notably `i_blocks`, `i_frag`/`i_fsize`, and the high halves of uid/gid.

Changed to `0` (Linux).

## Patch 2 — `i_blocks` lost the xattr block

`src/extent.rs` (both `write_extents` paths) and `src/formatter.rs` (inline-symlink path).

`Formatter::create()` charges an inode for a separate xattr block by *incrementing*
`blocks_lo`. `extent::write_extents` then ran `inode.blocks_lo = <data blocks>`, discarding
the charge; the inline-symlink branch separately did `blocks_lo = 0`. Net effect: `i_blocks`
short by one block whenever xattrs are carried, which `e2fsck` reports.

Changed both `write_extents` sites to `+=`, and dropped the inline-symlink reset.
`blocks_lo` starts at 0 and `write_extents` runs at most once per inode, so accumulation is
equivalent everywhere the bug did not apply.

Latent upstream (their `unpack_tar` always passes `xattrs: None`), live for us.

## Patch 3 — quadratic directory lookup

`src/file_tree.rs`.

`FileTree::lookup` resolved each path component with a linear scan of the parent's
`children`. `Formatter::create()` performs at least two lookups per entry, so building an
image was O(entries x directory width). Real rootfs directories are thousands of entries
wide (`/usr/lib/x86_64-linux-gnu`).

Added a `HashMap<(parent_idx, name), child_idx>` index maintained by `add_child` /
`remove_child`, making lookup O(path depth). `add_child` uses `or_insert` so a duplicate
name under one parent still resolves to the *first* child, matching the scan it replaces;
`remove_child` repoints the index to the next same-named sibling if one remains. Only that
rare path still scans.

## Patch 4 — wall clock read for the formatter's own inodes

`src/formatter.rs`: `FormatOptions.epoch`, plus its use for the root inode and `/lost+found`.

`Formatter::create()` already takes explicit `FileTimestamps`, but two inodes are created by the
formatter itself and read the clock directly: the root directory (via `Inode::root_inode()`) and
`/lost+found` (created with `ts: None`, which falls back to `FileTimestamps::default()`). That made
byte-reproducible output impossible from outside the crate.

Added `FormatOptions.epoch: Option<(u32, u32)>`; when set, both inodes use it. `None` keeps
upstream behaviour, so this is additive.

With this, an explicit `uuid`, and per-entry timestamps taken from the tar, the ext4 body is
byte-reproducible — verified across runs seconds apart.

Note the remaining nondeterminism is *outside* the crate: `tune2fs -j` stamps the journal inode and
the superblock's `s_wtime`. vmbuild's `ext4::finish::normalize_timestamps` undoes both. `s_wtime`
specifically cannot be fixed via `debugfs`, because `debugfs` rewrites it when closing the
filesystem; it is patched directly at its fixed offset, guarded on `metadata_csum` being absent.

## Patch 5 — image size rounded up to a whole block group

`src/formatter.rs`: `FormatOptions.allow_partial_final_group`, plus three changes in `close()`.

Upstream always grew the image to a whole ext4 block group (32768 blocks x 4096 = 128 MiB), so a
129 MiB request produced a 256 MiB file. `mke2fs` permits a short final group. The extra apparent
bytes are not free downstream: heyvm's per-VM copy is dense on ext4 hosts (no reflink), and
`heyvm mvm build` streams the image to S3, where holes upload as zeros.

Three parts, all gated on the new flag so `false` keeps upstream behaviour:

1. Skip the round-up to a group boundary.
2. In the data-bearing group loop, when the last group is short, mark the blocks past the end of
   the filesystem as allocated in its block bitmap. Upstream only did this for trailing *empty*
   groups — code that was unreachable while the size was always rounded up.
3. Take `blocks_count` from the real size rather than `groups x blocks_per_group`, so the
   superblock does not claim blocks past the end of the file.

Three further rules, **every one of them found by testing rather than by reading the code**:

4. *Minimum viable tail.* A sweep across requested sizes showed 129/130/257 MiB producing
   filesystems `e2fsck` rejected while 153/200/255 were clean. A partial final group must still
   hold its own metadata — two bitmap blocks plus a full inode table (512 blocks) = 514 blocks;
   a shorter tail describes an inode table running past the end of the filesystem. Tails below
   514 blocks are grown to exactly 514 (~2 MiB), not to the next 128 MiB boundary.

5. *A second round-up, in a different place.* `close()` also forced the image up to `min_groups`
   **whole** groups as soon as the written data crossed a group boundary. Every sizing test used a
   few-KB payload that fit inside group 0, so nothing caught it until a real 123 MiB rootfs came
   through heyvm and produced 256 MiB where 212 MiB was asked for. When partial groups are allowed
   the clamp now only guarantees the image covers what was actually written (`data_size`, which
   already counts data plus both bitmap blocks per group).

6. *Phantom blocks must not be counted as used.* Marking the tail past the end of the filesystem
   as allocated is right — that is ext4's bitmap padding convention — but adding those bits to
   `used_blocks` made `total_used_blocks` exceed the real block count, which drove the superblock's
   free-block count into a u32 underflow (`Free blocks: 4294963200`) and made `blocks_count` claim
   8 blocks the device did not have. The padding bits are now set without being counted, and each
   group's free count is measured against the blocks that group really contains.

The image size is also aligned up to a whole block: these files are attached to VMs as block
devices, and the caller's size heuristic need not land on a multiple of 4096 — or even of 512.

Net effect on a debian-slim rootfs: 256 MiB apparent -> **153 MiB**, identical to what
`fakeroot mke2fs -d` produces from the same heuristic; on the larger `Dockerfile.firecracker-debian`
rootfs, 256 MiB -> **212 MiB**. Verified across 12 sizes, a payload spanning two block groups, and
a real Firecracker boot.

---

## Known-unfixed, tracked

- **No `has_journal`.** Upstream writes no journal; the existing heyvm catalog images all
  have one (16M). Handled outside the fork by running `tune2fs -j` after `close()` — cheaper
  and lower-risk than implementing jbd2. See the plan's M1.
- **No `resize_inode`.** heyvm grows rootfs images ~100x (`resize2fs` to 20 GiB). Offline
  `resize2fs` turns out to work without it — verified in M2 on the exact `set_len` ->
  `e2fsck -fp` -> `resize2fs` path `kvm.rs` uses, and pinned by
  `survives_offline_resize_to_20_gib`. Left unfixed deliberately.
- **No device nodes.** `Formatter::create()` branches only on dir/symlink/regular;
  `S_IFCHR`/`S_IFBLK`/`S_IFIFO`/`S_IFSOCK` are unhandled and `src/unpack.rs` drops them via a
  bare `_ => continue`. Measured non-issue for real images (M0 found zero special files
  across 4,225 entries of a debian rootfs; `/dev` is an empty dir), but vmbuild counts and
  surfaces skipped entries rather than dropping them silently.
- **`close()` only grows `size`, never shrinks it.** The caller must still know the final size up
  front, which is why vmbuild exports the rootfs tar to a file (to stat it) instead of streaming
  from a pipe. The logic in `close()` already computes the true minimum from actual usage, so an
  `auto_size` mode remains a contained follow-up; it would allow true single-pass streaming.
- **Unconditional `HUGE_FILE` inode flag**, empty groups using a different inode-table layout
  than data-bearing groups, and `SPARSE_SUPER2` set with `s_backup_bgs` zeroed and no backup
  superblocks written. Audit items; not yet shown to be wrong.
- **No bounds check** that `create()`'s writes stay within `size`.
