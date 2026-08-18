use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;
use vmbuild::build::{BuildRequest, CacheHit};
use vmbuild::buildkit::{BuildSpec, ContextSource, DockerfileSource};
use vmbuild::ext4::{Ext4Options, SizePolicy, finish, write_ext4_from_tar};
use vmbuild::store::{GcPolicy, Store};

#[derive(Parser)]
#[command(name = "vmbuild", about = "Fast, content-addressed VM rootfs builder")]
struct Cli {
    /// Store root. Defaults to $VMBUILD_STORE, else $MVM_DATA_DIR/vmbuild,
    /// else ~/.heyo/vmbuild.
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
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
            let store = Store::open(&store_root)?;
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

            let outcome = vmbuild::build::build(&req, &store)?;
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

        Cmd::Cache { cmd } => {
            let store = Store::open(&store_root)?;
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
