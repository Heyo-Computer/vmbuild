//! The orchestrator: Dockerfile in, cached ext4 out.
//!
//! ```text
//!   buildx --load  ->  diffIDs  ->  key
//!                                    |- hit  -> install from the store
//!                                    `- miss -> buildx -o type=tar
//!                                               -> write_ext4_from_tar
//!                                               -> store.put
//! ```
//!
//! There is exactly one cache key, derived from what BuildKit actually built.
//! M0 measured the alternative -- hashing the build context ourselves to skip
//! the first invocation -- at 5.33s on a real 45k-file context versus 0.18s to
//! just ask BuildKit, so it is not done. That also removes the only place
//! where a bug could serve a *wrong* image rather than a slow one.

use crate::buildkit::{self, BuildSpec, ContextSource, DockerfileSource, Prepared};
use crate::error::{Error, Result};
use crate::ext4::{Ext4Options, Ext4Stats, SizePolicy, write_ext4_from_tar};
use crate::key::RecipeKey;
use crate::store::{InstallKind, Store};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum CacheHit {
    Miss,
    Hit,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Timings {
    pub solve_ms: u128,
    pub export_ms: u128,
    pub ext4_ms: u128,
    pub install_ms: u128,
    pub total_ms: u128,
}

#[derive(Debug, serde::Serialize)]
pub struct BuildOutcome {
    pub key: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub cache: CacheHit,
    pub install: Option<InstallKind>,
    pub timings: Timings,
    pub stats: Option<Ext4Stats>,
    pub diff_ids: Vec<String>,
}

pub struct BuildRequest {
    pub dockerfile: DockerfileSource,
    pub context: ContextSource,
    pub spec: BuildSpec,
    pub ext4: Ext4Options,
    /// Where to publish the finished image, if anywhere.
    pub install_to: Option<PathBuf>,
    /// Ignore an existing cache entry and rebuild the ext4.
    pub refresh: bool,
}

/// Extract an uploaded context tarball, refusing entries that escape it.
///
/// `tar-rs`'s `unpack_in` already refuses to write outside the destination;
/// the explicit check here fails with the offending entry named, and does so
/// before any write.
fn extract_context(bytes: &[u8], dest: &Path) -> Result<()> {
    let reader = decode(bytes);
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let escapes = path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)));
        if escapes {
            return Err(Error::PathEscape { path });
        }
        entry.unpack_in(dest)?;
    }
    Ok(())
}

/// Accept the context either gzipped (what heyvm's HTTP API sends) or plain,
/// decided by the gzip magic rather than by the caller asserting which it is.
fn decode(bytes: &[u8]) -> Box<dyn std::io::Read + '_> {
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        Box::new(flate2::read::GzDecoder::new(std::io::Cursor::new(bytes)))
    } else {
        Box::new(std::io::Cursor::new(bytes))
    }
}

pub fn build(req: &BuildRequest, store: &Store) -> Result<BuildOutcome> {
    let t_all = Instant::now();
    let mut timings = Timings::default();

    let prepared = buildkit::prepare(&req.dockerfile, &req.context)?;
    if let ContextSource::TarGz(bytes) = &req.context {
        extract_context(bytes, &prepared.context)?;
    }

    // 1. Ask BuildKit what this Dockerfile builds to.
    let t = Instant::now();
    let diff_ids = buildkit::solve(&prepared, &req.spec)?;
    timings.solve_ms = t.elapsed().as_millis();

    let recipe = RecipeKey {
        diff_ids: diff_ids.clone(),
        size_policy: match &req.ext4.size {
            SizePolicy::Fixed(n) => format!("fixed:{n}"),
            SizePolicy::FromTar { .. } => "auto".to_string(),
        },
        label: req.ext4.label.clone(),
        journal: req.ext4.journal,
        epoch_secs: req.ext4.epoch_secs,
    };
    let key = recipe.digest();

    // Serialize concurrent builds of the same key.
    let _guard = store.lock(&key)?;

    // 2. Cache hit?
    let mut cache = CacheHit::Hit;
    let mut stats = None;
    let blob = match store.get(&key).filter(|_| !req.refresh) {
        Some(p) => p,
        None => {
            cache = CacheHit::Miss;
            let (p, s) = build_uncached(&prepared, req, store, &recipe, &key, &mut timings)?;
            stats = Some(s);
            p
        }
    };

    let size_bytes = std::fs::metadata(&blob)?.len();

    // 3. Publish.
    let mut install = None;
    let mut path = blob;
    if let Some(dest) = &req.install_to {
        let t = Instant::now();
        install = Some(store.install(&key, dest)?);
        timings.install_ms = t.elapsed().as_millis();
        path = dest.clone();
    }

    timings.total_ms = t_all.elapsed().as_millis();
    Ok(BuildOutcome {
        key,
        path,
        size_bytes,
        cache,
        install,
        timings,
        stats,
        diff_ids,
    })
}

fn build_uncached(
    prepared: &Prepared,
    req: &BuildRequest,
    store: &Store,
    recipe: &RecipeKey,
    key: &str,
    timings: &mut Timings,
) -> Result<(PathBuf, Ext4Stats)> {
    let tmp = tempfile::tempdir()?;
    let tar_path = tmp.path().join("rootfs.tar");

    let t = Instant::now();
    let tar_bytes = buildkit::export_rootfs_tar(prepared, &req.spec, &tar_path)?;
    timings.export_ms = t.elapsed().as_millis();

    let mut opts = req.ext4.clone();
    // "auto" resolves against the tar we just exported.
    if matches!(opts.size, SizePolicy::FromTar { .. }) {
        opts.size = SizePolicy::FromTar { tar_bytes };
    }
    // Derive the UUID from the key: reproducible, and two recipes never
    // collide on filesystem UUID.
    opts.uuid = Some(recipe.uuid());

    // Build straight into the store's filesystem so `put` is a rename.
    let staging = store.staging_path(key);
    let t = Instant::now();
    let stats = write_ext4_from_tar(std::fs::File::open(&tar_path)?, &staging, &opts)?;
    timings.ext4_ms = t.elapsed().as_millis();

    let blob = store.put(key, &staging, &recipe.diff_ids)?;
    Ok((blob, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_extraction_refuses_escapes() {
        let d = tempfile::tempdir().unwrap();
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_size(3);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        // Bypass tar::Builder's own refusal by writing the name directly.
        let name = &mut h.as_old_mut().name;
        name.fill(0);
        name[.."../evil".len()].copy_from_slice(b"../evil");
        h.set_cksum();
        b.append(&h, std::io::Cursor::new(b"bad".to_vec())).unwrap();
        let bytes = b.into_inner().unwrap();

        assert!(matches!(
            extract_context(&bytes, d.path()),
            Err(Error::PathEscape { .. })
        ));
    }

    #[test]
    fn plain_context_extracts() {
        let d = tempfile::tempdir().unwrap();
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_size(2);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        b.append_data(&mut h, "ok.txt", std::io::Cursor::new(b"hi".to_vec()))
            .unwrap();
        let bytes = b.into_inner().unwrap();
        extract_context(&bytes, d.path()).unwrap();
        assert_eq!(std::fs::read(d.path().join("ok.txt")).unwrap(), b"hi");
    }
}
