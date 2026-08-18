//! vmbuild -- a fast, content-addressed VM rootfs builder.
//!
//! Replaces heyvm's `docker build | docker export | fakeroot tar | fakeroot
//! mke2fs | copy | copy` pipeline, which writes and reads the whole rootfs
//! about five times and caches none of it past `docker build`.
//!
//! The layering is deliberate: [`ext4::write_ext4_from_tar`] is a pure
//! function from a rootfs tar to an ext4 image with no Docker involvement at
//! all, so the part where a bug is silent and catastrophic can be tested,
//! fuzzed and differentially compared against `mke2fs -d` on its own.

pub mod build;
pub mod buildkit;
pub mod error;
pub mod ext4;
pub mod key;
pub mod store;

pub use build::{BuildOutcome, BuildRequest, CacheHit};
pub use buildkit::{BuildSpec, ContextSource, DockerfileSource};
pub use error::{Error, Result};
pub use ext4::{Ext4Options, Ext4Stats, SizePolicy, write_ext4_from_tar};
pub use key::RecipeKey;
pub use store::{GcPolicy, InstallKind, Store};
