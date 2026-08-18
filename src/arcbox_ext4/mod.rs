//! Vendored fork of [`arcbox-ext4`] 0.1.2 — a pure-Rust ext4 formatter.
//!
//! Copyright (c) 2026 ArcBox Labs, MIT OR Apache-2.0. The upstream licences
//! are kept verbatim beside this file as `LICENSE-MIT` / `LICENSE-APACHE`.
//!
//! This is a **fork, not a copy**: see `PATCHES.md` in this directory for the
//! five changes and why each is required. Two of them add public API
//! (`FormatOptions::epoch`, `FormatOptions::allow_partial_final_group`) that
//! upstream does not have, and two are silent correctness fixes, so vmbuild
//! cannot build against the crates.io release. It is carried here rather than
//! as a path dependency so vmbuild is publishable as a single crate.
//!
//! The file layout is kept 1:1 with upstream's `src/` so that rebasing onto a
//! future release stays a directory diff.
//!
//! [`arcbox-ext4`]: https://github.com/arcboxlabs/ext4-rs

// Upstream is a library crate, so every item is `pub` and reachable. Nested
// privately inside vmbuild, the parts we do not call (the `Reader`, the
// feature-flag constants for features we do not emit) are dead code. Silenced
// per-module rather than deleted: keeping the tree identical to upstream is
// what makes a future rebase a directory diff, and these warnings would
// otherwise drown out vmbuild's own.
#![allow(dead_code, unused_imports)]
// Likewise for style lints: this is third-party code kept close to upstream.
// Fixing its idioms here would inflate the rebase diff for no behavioural gain.
#![allow(clippy::all)]

//! Pure-Rust ext4 filesystem formatter and reader.
//!
//! This crate creates and reads ext4 filesystem images entirely in userspace,
//! with no kernel mount, no FUSE, and no C dependencies.  It is designed for
//! converting OCI container image layers into bootable block-device images.
//!
//! # Quick start
//!
//! Upstream's example, kept verbatim for reference. `ignore`d rather than run:
//! it imports `arcbox_ext4` as an external crate, which it no longer is here.
//!
//! ```ignore
//! use std::path::Path;
//! use arcbox_ext4::Formatter;
//!
//! // Create a new ext4 image.
//! let mut fmt = Formatter::new(Path::new("rootfs.ext4"), 4096, 256 * 1024).unwrap();
//! fmt.create("/hello.txt", 0x8000 | 0o644, None, None,
//!     Some(&mut "hello world".as_bytes()), None, None, None).unwrap();
//! fmt.close().unwrap();
//!
//! // Read it back.
//! let mut reader = arcbox_ext4::Reader::new(Path::new("rootfs.ext4")).unwrap();
//! let data = reader.read_file("/hello.txt", 0, None).unwrap();
//! assert_eq!(&data, b"hello world");
//! ```

pub mod constants;
pub mod dir;
pub mod error;
pub mod extent;
pub mod file_tree;
pub mod formatter;
pub mod reader;
pub mod reader_io;
pub mod types;
pub mod unpack;
pub mod xattr;

// Re-export the primary public types at the crate root.
pub use formatter::{FileTimestamps, FormatOptions, Formatter};
pub use reader::Reader;
