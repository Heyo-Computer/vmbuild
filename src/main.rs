use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;
use vmbuild::build::{BuildRequest, CacheHit};
use vmbuild::buildkit::{BuildSpec, ContextSource, DockerfileSource};
use vmbuild::ext4::{Ext4Options, SizePolicy, finish, write_ext4_from_tar};
use vmbuild::store::{GcPolicy, StorageBackend, Store};

#[derive(Parser)]
#[command(name = "vmbuild", about = "Fast, content-addressed VM rootfs builder")]
struct Cli {
    /// Store root. Defaults to $VMBUILD_STORE, else $MVM_DATA_DIR/vmbuild,
    /// else ~/.heyo/vmbuild.
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    /// Storage backend. `posix` (default) uses files, hardlinks and FICLONE.
    ///
    /// `zfs` keeps a dataset per image and clones it per materialization.
    /// Never selected automatically -- a machine that happens to sit on a
    /// zpool must not silently change behaviour. Requires the `zfs` feature,
    /// and root: Linux cannot delegate the `mount` permission that
    /// zfs create/clone/destroy all need.
    #[arg(long, global = true, default_value = "posix")]
    backend: String,
    /// Parent dataset for `--backend zfs`, e.g. `tank/vmbuild`.
    #[arg(long, global = true)]
    zfs_dataset: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

/// Build the requested backend. Errors clearly rather than falling back --
/// silently using a different backend than asked for is worse than failing.
fn open_backend(
    kind: &str,
    root: &Path,
    zfs_dataset: Option<&str>,
) -> vmbuild::Result<Box<dyn StorageBackend>> {
    match kind {
        "posix" => Ok(Box::new(Store::open(root)?)),
        #[cfg(feature = "zfs")]
        "zfs" => {
            let ds = zfs_dataset.ok_or_else(|| {
                vmbuild::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--backend zfs requires --zfs-dataset (e.g. tank/vmbuild)",
                ))
            })?;
            Ok(Box::new(vmbuild::zfs::ZfsBackend::new(
                vmbuild::zfs::SystemZfs,
                ds,
                root,
            )))
        }
        #[cfg(not(feature = "zfs"))]
        "zfs" => {
            let _ = zfs_dataset;
            Err(vmbuild::Error::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "this build has no ZFS support; rebuild with --features zfs \
                 (experimental, and root-only on Linux)",
            )))
        }
        other => Err(vmbuild::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown backend {other:?}; expected \"posix\" or \"zfs\""),
        ))),
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Build an ext4 rootfs from a Dockerfile, using the content-addressed cache.
    Build {
        #[arg(short = 'f', long, default_value = "Dockerfile")]
        file: PathBuf,
        /// Build context. Defaults to the Dockerfile's directory.
        context: Option<PathBuf>,
        /// Write the image here.
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Install into heyvm's catalog as <name>.ext4.
        #[arg(short = 'n', long)]
        name: Option<String>,
        #[arg(long)]
        size_mb: Option<u64>,
        #[arg(long = "build-arg", value_parser = parse_kv)]
        build_arg: Vec<(String, String)>,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        platform: Option<String>,
        #[arg(long)]
        no_cache: bool,
        #[arg(long)]
        pull: bool,
        /// Rebuild the ext4 even if the cache has it.
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Write an ext4 image from a rootfs tar. No Docker involved -- this is
    /// the seam the correctness suite drives.
    Ext4 {
        /// Source tar; `-` reads stdin.
        #[arg(long = "from-tar")]
        from_tar: String,
        #[arg(short = 'o', long)]
        out: PathBuf,
        /// Image size in MiB. Default derives from the tar size.
        #[arg(long)]
        size_mb: Option<u64>,
        #[arg(long, default_value = "rootfs")]
        label: String,
        /// Skip the journal. Only for experiments -- see finish::add_journal.
        #[arg(long)]
        no_journal: bool,
        /// Fail if the tar contains device nodes, FIFOs or sockets.
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        json: bool,
    },
    /// Produce a writable private copy of a stored image.
    ///
    /// Uses FICLONE where the filesystem supports it, otherwise a
    /// hole-preserving copy. Unlike `build --out`, which hardlinks a
    /// read-only catalog name, this yields an independent writable file --
    /// what a VM needs for its own rootfs.
    Materialize {
        /// Cache key, as shown by `vmbuild cache ls`.
        key: String,
        /// Destination path.
        dest: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Report what this machine will do with a copy: does FICLONE work between
    /// the store and a destination, and what will vmbuild fall back to?
    ///
    /// Advisory only, and creates nothing -- not even the store.
    Doctor {
        /// Directory a per-VM copy would be written to. Defaults to the store,
        /// which reports what same-filesystem copies will do.
        #[arg(long)]
        dest: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Reclaim a copy previously produced by `materialize`.
    ///
    /// On a plain filesystem this is an unlink. It matters on a backend where
    /// a materialization pins the image it came from -- a ZFS clone holds its
    /// origin snapshot, so without this the store can never reclaim the blob.
    Release {
        /// Path previously passed to `materialize`.
        dest: PathBuf,
    },
    /// Inspect and prune the image store.
    Cache {
        #[command(subcommand)]
        cmd: CacheCmd,
    },
    /// Run `e2fsck -fn` against an image and report its feature set.
    Verify {
        image: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// List stored images, most recently used first.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Evict least-recently-used images. Entries still hardlinked into a
    /// catalog are never evicted.
    Gc {
        /// Keep the store under this many MiB.
        #[arg(long)]
        max_mb: Option<u64>,
        /// Evict entries unused for longer than this many days.
        #[arg(long)]
        keep_days: Option<u64>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

fn parse_kv(s: &str) -> std::result::Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) => Ok((k.to_string(), v.to_string())),
        None => Err(format!("expected KEY=VALUE, got {s:?}")),
    }
}

fn mib(n: u64) -> String {
    format!("{:.1} MiB", n as f64 / 1048576.0)
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vmbuild: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> vmbuild::Result<()> {
    let cli = Cli::parse();
    let store_root = cli.store.clone().unwrap_or_else(Store::default_root);
    let backend = cli.backend.clone();
    let zfs_dataset = cli.zfs_dataset.clone();
    let open = |root: &Path| open_backend(&backend, root, zfs_dataset.as_deref());

    match cli.cmd {
        Cmd::Build {
            file,
            context,
            out,
            name,
            size_mb,
            build_arg,
            target,
            platform,
            no_cache,
            pull,
            refresh,
            json,
        } => {
            let store = open(&store_root)?;
            let file = file.canonicalize().unwrap_or(file);
            let ctx = context.unwrap_or_else(|| {
                file.parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            });

            let install_to = match (&out, &name) {
                (Some(o), _) => Some(o.clone()),
                (None, Some(n)) => Some(heyvm_catalog_dir().join(format!("{n}.ext4"))),
                (None, None) => None,
            };

            let req = BuildRequest {
                dockerfile: DockerfileSource::Path(file),
                context: ContextSource::Dir(ctx),
                spec: BuildSpec {
                    target,
                    build_args: build_arg.into_iter().collect::<BTreeMap<_, _>>(),
                    platform,
                    no_cache,
                    pull,
                },
                ext4: Ext4Options {
                    size: match size_mb {
                        Some(mb) => SizePolicy::Fixed(mb * 1024 * 1024),
                        None => SizePolicy::FromTar { tar_bytes: 0 },
                    },
                    ..Default::default()
                },
                install_to,
                refresh,
            };

            let outcome = vmbuild::build::build(&req, store.as_ref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&outcome).unwrap());
            } else {
                let tag = match outcome.cache {
                    CacheHit::Hit => "cache hit",
                    CacheHit::Miss => "built",
                };
                println!(
                    "{tag} in {:.2}s -> {}",
                    outcome.timings.total_ms as f64 / 1000.0,
                    outcome.path.display()
                );
                println!(
                    "  key {}  size {}",
                    &outcome.key[..16],
                    mib(outcome.size_bytes)
                );
                println!(
                    "  solve {}ms  export {}ms  ext4 {}ms  install {}ms",
                    outcome.timings.solve_ms,
                    outcome.timings.export_ms,
                    outcome.timings.ext4_ms,
                    outcome.timings.install_ms
                );
                if let Some(k) = outcome.install {
                    println!("  installed by {k:?}");
                }
                if let Some(s) = &outcome.stats
                    && !s.skipped_special.is_empty()
                {
                    eprintln!(
                        "  warning: {} special file(s) skipped (device nodes/FIFOs/sockets)",
                        s.skipped_special.len()
                    );
                }
            }
            Ok(())
        }

        Cmd::Ext4 {
            from_tar,
            out,
            size_mb,
            label,
            no_journal,
            strict,
            json,
        } => {
            // Sizing has to be decided before the first byte is written:
            // arcbox needs `size` at construction and its `close()` only ever
            // grows it. From a file we can stat; from stdin we cannot, so the
            // caller must pass --size-mb.
            let (reader, tar_bytes): (Box<dyn Read>, Option<u64>) = if from_tar == "-" {
                (Box::new(std::io::stdin().lock()), None)
            } else {
                let p = PathBuf::from(&from_tar);
                let n = std::fs::metadata(&p)?.len();
                (Box::new(std::fs::File::open(&p)?), Some(n))
            };

            let size = match (size_mb, tar_bytes) {
                (Some(mb), _) => SizePolicy::Fixed(mb * 1024 * 1024),
                (None, Some(n)) => SizePolicy::FromTar { tar_bytes: n },
                (None, None) => {
                    eprintln!(
                        "vmbuild: --size-mb is required when reading the tar from stdin \
                         (the image size must be fixed before writing begins)"
                    );
                    return Ok(());
                }
            };

            let opts = Ext4Options {
                size,
                label: Some(label),
                uuid: Some(uuid::Uuid::from_u128(
                    0x766d_6275_696c_6400_0000_0000_0000_0001,
                )),
                journal: !no_journal,
                strict_special_files: strict,
                epoch_secs: vmbuild::ext4::default_epoch(),
            };

            let t0 = Instant::now();
            let stats = write_ext4_from_tar(reader, &out, &opts)?;
            let elapsed = t0.elapsed();

            if json {
                let v = serde_json::json!({
                    "out": out,
                    "elapsed_ms": elapsed.as_millis(),
                    "stats": stats,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!(
                    "wrote {} in {:.2}s\n  {} files, {} dirs, {} symlinks, {} hardlinks\n  \
                     apparent {}, actual {}",
                    out.display(),
                    elapsed.as_secs_f64(),
                    stats.files,
                    stats.dirs,
                    stats.symlinks,
                    stats.hardlinks,
                    mib(stats.apparent_size),
                    mib(stats.actual_size),
                );
                if !stats.skipped_special.is_empty() {
                    eprintln!(
                        "  warning: {} special file(s) skipped (device nodes/FIFOs/sockets); \
                         re-run with --strict to make this fatal",
                        stats.skipped_special.len()
                    );
                    for p in stats.skipped_special.iter().take(5) {
                        eprintln!("    {}", p.display());
                    }
                }
            }
            Ok(())
        }

        Cmd::Materialize { key, dest, json } => {
            let store = open(&store_root)?;
            let t0 = Instant::now();
            let m = store.materialize(&key, &dest)?;
            let elapsed = t0.elapsed();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "key": key, "dest": dest,
                        "result": m, "elapsed_ms": elapsed.as_millis(),
                    }))
                    .unwrap()
                );
            } else {
                let how = match m {
                    vmbuild::Materialization::Cloned { .. } => "cloned (CoW)",
                    _ => "copied (sparse)",
                };
                println!(
                    "{how} -> {} in {:.2}s, {} written",
                    dest.display(),
                    elapsed.as_secs_f64(),
                    mib(m.bytes_written())
                );
            }
            Ok(())
        }

        Cmd::Doctor { dest, json } => {
            // Deliberately does not call Store::open: a diagnostic that creates
            // four directories as a side effect is a bug. Probe the store root
            // if it exists, else its parent, else the cwd.
            let src = if store_root.is_dir() {
                store_root.clone()
            } else {
                store_root
                    .parent()
                    .filter(|p| p.is_dir())
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."))
            };
            let dst = dest.unwrap_or_else(|| src.clone());
            let r = vmbuild::doctor::run(&src, &dst)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r).unwrap());
            } else {
                println!("source {} ({})", r.source_dir.display(), r.source_fs);
                println!("dest   {} ({})", r.dest_dir.display(), r.dest_fs);
                println!(
                    "  block sharing (FICLONE): {}",
                    match r.reflink {
                        Some(true) => "yes",
                        Some(false) => "no",
                        None => "unknown",
                    }
                );
                println!("    {}", r.reflink_detail);
                if let Some(z) = &r.zfs {
                    println!("  openzfs {}", z.version);
                    println!(
                        "    zfs_bclone_enabled: {}",
                        match z.bclone_enabled {
                            Some(v) => v.to_string(),
                            None => "unknown".into(),
                        }
                    );
                    println!("    {}", z.note);
                }
                println!("\n{}", r.verdict);
            }
            Ok(())
        }

        Cmd::Release { dest } => {
            let store = open(&store_root)?;
            store.release(&dest)?;
            println!("released {}", dest.display());
            Ok(())
        }

        Cmd::Cache { cmd } => {
            let store = open(&store_root)?;
            match cmd {
                CacheCmd::Ls { json } => {
                    let entries = store.list()?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&entries).unwrap());
                    } else if entries.is_empty() {
                        println!("store {} is empty", store.root().display());
                    } else {
                        let total: u64 = entries.iter().map(|e| e.actual_bytes).sum();
                        println!("{} entries, {} on disk", entries.len(), mib(total));
                        for e in &entries {
                            println!(
                                "  {}  {:>10}  {} layer(s)",
                                &e.key[..16],
                                mib(e.actual_bytes),
                                e.diff_ids.len()
                            );
                        }
                    }
                    Ok(())
                }
                CacheCmd::Gc {
                    max_mb,
                    keep_days,
                    dry_run,
                    json,
                } => {
                    let policy = GcPolicy {
                        max_bytes: max_mb.map(|m| m * 1024 * 1024),
                        keep_secs: keep_days.map(|d| d * 86400),
                        dry_run,
                    };
                    if policy.max_bytes.is_none() && policy.keep_secs.is_none() {
                        eprintln!("vmbuild: pass --max-mb and/or --keep-days");
                        return Ok(());
                    }
                    let r = store.gc(&policy)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&r).unwrap());
                    } else {
                        println!(
                            "{} {} entries, freeing {} (of {}); {} still installed and kept",
                            if dry_run { "would remove" } else { "removed" },
                            r.removed.len(),
                            mib(r.freed_bytes),
                            mib(r.total_before),
                            r.kept_linked
                        );
                        if !r.kept_busy.is_empty() {
                            println!(
                                "  {} could not be removed and were left intact: {}",
                                r.kept_busy.len(),
                                r.kept_busy.join(", ")
                            );
                        }
                        if let Some(why) = &r.stopped_early {
                            println!("  stopped early: {why}");
                        }
                    }
                    Ok(())
                }
            }
        }

        Cmd::Verify { image, json } => {
            let features = finish::features(&image)?;
            let fsck = finish::fsck(&image);
            let ok = fsck.is_ok();
            if json {
                let v = serde_json::json!({
                    "image": image,
                    "fsck_clean": ok,
                    "features": features,
                    "detail": match &fsck { Ok(s) => s.clone(), Err(e) => e.to_string() },
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!("{}", image.display());
                println!("  features:   {}", features.join(" "));
                println!(
                    "  has_journal: {}",
                    features.iter().any(|f| f == "has_journal")
                );
                match &fsck {
                    Ok(_) => println!("  e2fsck -fn: clean"),
                    Err(e) => println!("  e2fsck -fn: FAILED\n{e}"),
                }
            }
            fsck.map(|_| ())
        }
    }
}

/// Where heyvm's Firecracker/KVM drivers look for images.
fn heyvm_catalog_dir() -> PathBuf {
    let data = std::env::var("MVM_DATA_DIR").unwrap_or_else(|_| {
        format!(
            "{}/.heyo",
            std::env::var("HOME").unwrap_or_else(|_| ".".into())
        )
    });
    PathBuf::from(data).join("images").join("firecracker")
}
