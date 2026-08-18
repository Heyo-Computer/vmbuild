//! Driving BuildKit through `docker buildx`.
//!
//! vmbuild parses **no** Dockerfile syntax. "Full Dockerfile compatibility" is
//! an argument for letting BuildKit's own frontend be the only interpreter --
//! any disagreement with it would itself be the bug. Everything here is
//! argument marshalling plus reading back the diffID chain.
//!
//! Two invocations per build:
//!
//!  1. `buildx build --load -t <tag>` (~0.16s warm), then
//!     `docker image inspect --format '{{json .RootFS.Layers}}'` (~0.02s) to
//!     get the diffIDs, which are the cache key.
//!  2. Only on a cache miss: `buildx build -o type=tar,dest=<file>`, which
//!     hands back a *flattened*, whiteout-resolved rootfs. BuildKit has
//!     already merged the layers, so vmbuild needs no whiteout tracking, no
//!     OCI layout parsing, and no layer-merge algorithm.

use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub enum DockerfileSource {
    Path(PathBuf),
    Text(String),
}

#[derive(Debug, Clone)]
pub enum ContextSource {
    Dir(PathBuf),
    /// A gzipped tar of the context, as heyvm's `POST /images/build` receives.
    TarGz(Vec<u8>),
    Empty,
}

#[derive(Debug, Clone, Default)]
pub struct BuildSpec {
    pub target: Option<String>,
    pub build_args: BTreeMap<String, String>,
    pub platform: Option<String>,
    pub no_cache: bool,
    pub pull: bool,
}

/// Dockerfile and context materialized on disk, ready to hand to buildx.
pub struct Prepared {
    pub dockerfile: PathBuf,
    pub context: PathBuf,
    _tmp: Option<tempfile::TempDir>,
}

pub fn prepare(df: &DockerfileSource, ctx: &ContextSource) -> Result<Prepared> {
    let needs_tmp = matches!(df, DockerfileSource::Text(_))
        || matches!(ctx, ContextSource::TarGz(_) | ContextSource::Empty);
    let tmp = if needs_tmp {
        Some(tempfile::tempdir()?)
    } else {
        None
    };
    let tmp_path = tmp.as_ref().map(|t| t.path().to_path_buf());

    let context = match ctx {
        ContextSource::Dir(d) => d.clone(),
        ContextSource::Empty => {
            let c = tmp_path.as_ref().unwrap().join("context");
            std::fs::create_dir_all(&c)?;
            c
        }
        ContextSource::TarGz(_bytes) => {
            let c = tmp_path.as_ref().unwrap().join("context");
            std::fs::create_dir_all(&c)?;
            // Extraction is handled by the caller-facing API in build.rs so
            // that the traversal checks live in one place.
            c
        }
    };

    // Written *next to* the context, never inside it, so an uploaded context
    // containing its own Dockerfile cannot displace the one we were given.
    let dockerfile = match df {
        DockerfileSource::Path(p) => {
            if !p.is_file() {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Dockerfile not found at {}", p.display()),
                )));
            }
            p.clone()
        }
        DockerfileSource::Text(t) => {
            if t.trim().is_empty() {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "dockerfile is empty",
                )));
            }
            let p = tmp_path.as_ref().unwrap().join("Dockerfile");
            std::fs::write(&p, t)?;
            p
        }
    };

    Ok(Prepared {
        dockerfile,
        context,
        _tmp: tmp,
    })
}

fn common_args(cmd: &mut Command, p: &Prepared, spec: &BuildSpec) {
    cmd.arg("buildx").arg("build");
    cmd.arg("-f").arg(&p.dockerfile);
    if let Some(t) = &spec.target {
        cmd.arg("--target").arg(t);
    }
    if let Some(pl) = &spec.platform {
        cmd.arg("--platform").arg(pl);
    }
    for (k, v) in &spec.build_args {
        cmd.arg("--build-arg").arg(format!("{k}={v}"));
    }
    if spec.no_cache {
        cmd.arg("--no-cache");
    }
    if spec.pull {
        cmd.arg("--pull");
    }
}

fn run(mut cmd: Command, what: &'static str) -> Result<String> {
    let out = cmd.output().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            Error::ToolMissing { tool: "docker" }
        } else {
            Error::Io(e)
        }
    })?;
    if !out.status.success() {
        return Err(Error::ToolFailed {
            tool: what,
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Build the image and return its diffID chain.
///
/// Loads under a throwaway tag, reads `RootFS.Layers`, then removes the tag.
/// The tag is only a handle for `docker image inspect`; BuildKit's own build
/// cache is what makes the next invocation fast, and that is unaffected by
/// untagging.
pub fn solve(p: &Prepared, spec: &BuildSpec) -> Result<Vec<String>> {
    let tag = format!("vmbuild-stage:{}", uuid::Uuid::new_v4().simple());

    let mut cmd = Command::new("docker");
    common_args(&mut cmd, p, spec);
    cmd.arg("--load").arg("-t").arg(&tag).arg(&p.context);
    run(cmd, "docker buildx build --load")?;

    let mut inspect = Command::new("docker");
    inspect
        .arg("image")
        .arg("inspect")
        .arg(&tag)
        .arg("--format")
        .arg("{{json .RootFS.Layers}}");
    let json = run(inspect, "docker image inspect");

    // Best-effort untag regardless of how the inspect went.
    let _ = Command::new("docker")
        .args(["image", "rm", "-f", &tag])
        .output();

    let json = json?;
    let ids: Vec<String> = serde_json::from_str(json.trim())
        .map_err(|e| Error::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
    if ids.is_empty() {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "image reported an empty layer chain",
        )));
    }
    Ok(ids)
}

/// Export the flattened rootfs as a tar. Returns its size in bytes.
///
/// Written to a file rather than streamed from a pipe because the ext4 size
/// must be fixed before the first byte is written (`arcbox_ext4`'s `close()`
/// only ever grows it), and the size heuristic is a function of the tar's
/// length.
pub fn export_rootfs_tar(p: &Prepared, spec: &BuildSpec, dest: &Path) -> Result<u64> {
    let mut cmd = Command::new("docker");
    common_args(&mut cmd, p, spec);
    cmd.arg("-o")
        .arg(format!("type=tar,dest={}", dest.display()))
        .arg(&p.context);
    run(cmd, "docker buildx build -o type=tar")?;
    Ok(std::fs::metadata(dest)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_dockerfile_is_written_outside_the_context() {
        let p = prepare(
            &DockerfileSource::Text("FROM scratch\n".into()),
            &ContextSource::Empty,
        )
        .unwrap();
        assert!(p.dockerfile.is_file());
        assert!(
            !p.dockerfile.starts_with(&p.context),
            "an uploaded context must not be able to shadow the Dockerfile"
        );
    }

    #[test]
    fn empty_dockerfile_text_is_rejected() {
        assert!(
            prepare(
                &DockerfileSource::Text("   \n".into()),
                &ContextSource::Empty
            )
            .is_err()
        );
    }

    #[test]
    fn missing_dockerfile_path_is_rejected() {
        assert!(
            prepare(
                &DockerfileSource::Path("/nonexistent/Dockerfile".into()),
                &ContextSource::Empty
            )
            .is_err()
        );
    }
}
