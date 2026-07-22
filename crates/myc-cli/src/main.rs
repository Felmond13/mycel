//! myc — the Mycel CLI.
//!
//! Mycel replaces opaque container images with content-addressed file
//! graphs: every file is stored once, environments are described by small
//! manifests, and rootfs materialization is hardlink-based (zero copy).

mod commands;
mod deploy_commands;
mod hub_commands;
mod ui;
#[cfg(windows)]
mod win;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "myc",
    version,
    about = "Mycel: content-addressed container runtime",
    long_about = "Mycel stores environments as content-addressed file graphs.\n\
                  Ingest any OCI/Docker image, deduplicate it file-by-file,\n\
                  and run it rootless — no daemon, no root, no image blobs."
)]
struct Cli {
    /// Store location (default: $MYCEL_STORE or ~/.mycel)
    #[arg(long, global = true)]
    store: Option<std::path::PathBuf>,
    /// Windows only: run natively against a host-side store instead of
    /// proxying to the Linux twin inside WSL2 (portable commands only —
    /// containers need Linux)
    #[arg(long, global = true)]
    no_proxy: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Pull an OCI/Docker image and ingest it as a Mycel manifest
    Ingest {
        /// Image reference, e.g. `alpine:3.20`, `ghcr.io/org/app:v1`
        image: String,
        /// Target platform architecture (default: host)
        #[arg(long)]
        arch: Option<String>,
    },
    /// Ensure every blob of a manifest is present locally (offline guarantee)
    Pull {
        /// Manifest id, ref name or image reference
        reference: String,
        /// Pull from a Mycel hub instead of a registry (default: $MYCEL_HUB)
        #[arg(long)]
        from: Option<String>,
    },
    /// Run an environment
    Run {
        /// Manifest id, ref name or image reference (auto-ingested if unknown)
        reference: String,
        /// Command override
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
        /// Extra environment variables (KEY=VALUE)
        #[arg(short, long)]
        env: Vec<String>,
        /// Mounts SOURCE:CONTAINER[:ro] — SOURCE is a host path
        /// (/host/dir) or a named volume (mydata), created on first use
        #[arg(short = 'v', long = "volume")]
        binds: Vec<String>,
        /// Container hostname
        #[arg(long, default_value = "mycel")]
        hostname: String,
        /// Working directory inside the container
        #[arg(short, long)]
        workdir: Option<String>,
        /// Network mode: `host` (default — the container shares this
        /// machine's network), `isolated` (own private network via pasta;
        /// open ports with -p) or `none` (no connectivity at all)
        #[arg(long, default_value = "host")]
        net: String,
        /// Publish a port into an isolated container (repeatable):
        /// `HOST:CONTAINER` or just `PORT`. Implies --net isolated.
        /// Default with --net isolated: the image's exposed ports.
        #[arg(short = 'p', long = "publish")]
        publish: Vec<String>,
        /// (internal) Join the user+network namespaces of this pod-holder
        /// process (stacks in pod mode) instead of creating fresh ones.
        #[arg(long, hide = true)]
        join_net: Option<i32>,
        /// Deprecated alias of `--net none`
        #[arg(long, hide = true)]
        isolated_network: bool,
        /// Keep the materialized rootfs after exit
        #[arg(long)]
        keep_rootfs: bool,
        /// Start instantly; stream missing files on demand from a hub
        #[arg(long)]
        lazy: bool,
        /// Hub URL for --lazy (default: $MYCEL_HUB)
        #[arg(long)]
        from: Option<String>,
        /// (internal) Materialize into this directory instead of a temp dir;
        /// the caller owns the directory's lifecycle.
        #[arg(long, hide = true)]
        run_dir: Option<std::path::PathBuf>,
        /// Map the host user to this UID[:GID] inside the container instead
        /// of root (e.g. `--map-user 999` to run as the postgres user)
        #[arg(long)]
        map_user: Option<String>,
    },
    /// Build an image natively from a mycel-build.toml (no Dockerfile, no daemon)
    Build {
        /// Build file
        #[arg(short, long, default_value = "mycel-build.toml")]
        file: std::path::PathBuf,
        /// Result name (NAME:TAG), overrides `build.name` from the file
        #[arg(short = 't', long = "tag")]
        tag: Option<String>,
        /// Fixed build timestamp (RFC 3339 or unix seconds) for reproducible
        /// ids; also honored via $SOURCE_DATE_EPOCH (default: now)
        #[arg(long)]
        timestamp: Option<String>,
    },
    /// List stored environments
    Ls {
        /// Also print each manifest's full id and its JSON path on disk
        #[arg(long)]
        paths: bool,
    },
    /// Show details of a stored environment (files, size, config)
    Inspect {
        reference: String,
        /// Print the full file list
        #[arg(long)]
        files: bool,
    },
    /// Remove a stored environment (its manifest and refs; blobs are GC'd separately)
    Rm { reference: String },
    /// Delete unreferenced blobs from the store
    Gc,
    /// Pin an environment so garbage collection never touches its blobs
    Pin { reference: String },
    /// Remove a pin
    Unpin { reference: String },
    /// Store statistics: dedup ratio, physical vs logical size
    Stats,
    /// Export an environment (manifest + blobs) to a portable archive
    Export {
        reference: String,
        /// Output file (default: <name>.mycel.tar.gz, or <name>.oci.tar with --oci)
        #[arg(value_name = "OUTPUT")]
        output_pos: Option<std::path::PathBuf>,
        /// Output file (same as the positional OUTPUT)
        #[arg(short, long, conflicts_with = "output_pos")]
        output: Option<std::path::PathBuf>,
        /// Standard OCI image — works with docker load, podman, Kubernetes.
        /// Full reversibility: any Mycel environment back to a Docker image.
        #[arg(long)]
        oci: bool,
    },
    /// Import an archive produced by `myc export`
    Import { archive: std::path::PathBuf },
    /// Start every service of a mycel.toml project (foreground)
    Up {
        /// Project file (default: ./mycel.toml)
        #[arg(short, long, default_value = "mycel.toml")]
        file: std::path::PathBuf,
        /// Override the project's network mode: `pod` gives the whole stack
        /// one private network (services meet on localhost, only the ports
        /// from [network] are published), `host` shares this machine's
        /// network (the default when the file has no [network] section)
        #[arg(long)]
        net: Option<String>,
    },
    /// Clean up a stack's private network (pod holder + pasta)
    Down {
        /// Project file (default: ./mycel.toml)
        #[arg(short, long, default_value = "mycel.toml")]
        file: std::path::PathBuf,
    },
    /// (internal) The pod-holder process: owns a stack's private network
    /// namespaces and sleeps until killed
    #[command(name = "_pod-holder", hide = true)]
    PodHolder {
        /// Die automatically when the process that spawned us dies
        #[arg(long)]
        die_with_parent: bool,
    },
    /// Verify the integrity of every blob referenced by a manifest
    Verify { reference: String },
    /// Compare two environments file-by-file (added/removed/modified)
    Diff {
        /// Baseline environment (e.g. `web:v1`)
        a: String,
        /// New environment (e.g. `web:v2`)
        b: String,
    },
    /// Which environments contain this blob or path? (reverse query)
    Which {
        /// BLAKE3 hash prefix (≥6 hex chars) or a path substring, e.g. `libssl`
        query: String,
    },
    /// Emit a machine-readable SBOM (JSON on stdout): packages + all files
    Sbom {
        /// Manifest id, ref name or image reference (auto-ingested if unknown)
        reference: String,
    },
    /// Drop into an interactive /bin/sh in any image (auto-ingests it)
    Shell {
        /// Image reference, e.g. `alpine:3.20`
        image: String,
    },
    /// Check that this machine can ingest and run environments
    Doctor,
    /// Manage named volumes (persistent app data under the store)
    #[command(subcommand)]
    Volume(VolumeCommand),
    /// Team hub: share this store over HTTP (push/pull only deltas)
    #[command(subcommand)]
    Hub(HubCommand),
    /// Push an environment to a hub (uploads only the blobs it is missing)
    Push {
        /// Manifest id or ref name (pushed under this name)
        reference: String,
        /// Hub URL (default: $MYCEL_HUB)
        #[arg(long)]
        to: Option<String>,
        /// Bearer token for hub writes (default: $MYCEL_HUB_TOKEN)
        #[arg(long)]
        token: Option<String>,
    },
    /// Open the local web dashboard
    Ui {
        /// Port to listen on (localhost only)
        #[arg(long, default_value_t = 7777)]
        port: u16,
    },
    /// Deploy an environment to any Linux server over SSH (delta only)
    Deploy {
        /// Manifest id or ref name (deployed under this name)
        reference: String,
        /// Destination: `user@host` or `ssh://user@host[:port]`
        target: String,
        /// Start the environment on the server after the transfer
        #[arg(long)]
        run: bool,
        /// Stop the previous instance of this ref first, then start (implies --run)
        #[arg(long)]
        restart: bool,
        /// Publish ports on the server (forwarded to the remote `myc run -p`)
        #[arg(short = 'p', long = "publish")]
        publish: Vec<String>,
        /// Mounts on the server (forwarded to the remote `myc run -v`)
        #[arg(short = 'v', long = "volume")]
        binds: Vec<String>,
        /// Network mode on the server (forwarded to the remote `myc run --net`)
        #[arg(long)]
        net: Option<String>,
        /// Path of the myc binary on the server (default: `myc` in PATH,
        /// then ~/.local/bin/myc)
        #[arg(long)]
        myc_path: Option<String>,
        /// Never upload a myc binary to the server when it has none
        #[arg(long)]
        no_bootstrap: bool,
    },
    /// (internal) Remote half of `myc deploy`: speak the deploy protocol
    /// on stdin/stdout
    #[command(name = "_serve-deploy", hide = true)]
    ServeDeploy,
}

#[derive(Subcommand)]
enum VolumeCommand {
    /// List volumes with their sizes
    Ls,
    /// Show a volume's metadata and host path
    Inspect { name: String },
    /// Delete a volume and all of its data
    Rm { name: String },
}

#[derive(Subcommand)]
enum HubCommand {
    /// Serve this store over HTTP for the whole team
    Serve {
        /// Port to listen on (all interfaces)
        #[arg(long, default_value_t = 9600)]
        port: u16,
        /// Require this bearer token for writes (default: $MYCEL_HUB_TOKEN; reads stay open)
        #[arg(long)]
        token: Option<String>,
    },
}

fn main() {
    // On Windows, myc.exe is (by default) a transparent front for a Linux
    // twin inside WSL2: almost every command is proxied verbatim before
    // clap even parses it. See `win::preflight` for the exact rules.
    #[cfg(windows)]
    let cli = match win::preflight() {
        win::Preflight::Exit(code) => std::process::exit(code),
        win::Preflight::Native(args) => Cli::parse_from(args),
    };
    #[cfg(not(windows))]
    let cli = Cli::parse();
    // --no-proxy is consumed before parsing on Windows and a no-op elsewhere.
    let _ = cli.no_proxy;
    let store_root = cli.store.unwrap_or_else(myc_store::Store::default_root);

    // Bare `myc` gets a guided welcome, not a usage error.
    let Some(command) = cli.command else {
        std::process::exit(commands::welcome(&store_root).unwrap_or(1));
    };

    let result = match command {
        Command::Ingest { image, arch } => commands::ingest(&store_root, &image, arch.as_deref()),
        Command::Pull { reference, from } => {
            // --from / $MYCEL_HUB pulls from a hub; otherwise the registry.
            if from.is_some() || std::env::var("MYCEL_HUB").map(|v| !v.is_empty()) == Ok(true) {
                hub_commands::pull_from_hub(&store_root, &reference, from.as_deref())
            } else {
                commands::pull(&store_root, &reference)
            }
        }
        Command::Run {
            reference,
            command,
            env,
            binds,
            hostname,
            workdir,
            net,
            publish,
            join_net,
            isolated_network,
            keep_rootfs,
            lazy,
            from,
            run_dir,
            map_user,
        } => {
            // The legacy --isolated-network flag meant "own namespace, no
            // connectivity" — exactly what --net none does today.
            let net = if isolated_network {
                "none".to_string()
            } else {
                net
            };
            if lazy {
                hub_commands::run_lazy(
                    &store_root,
                    &reference,
                    hub_commands::LazyRunArgs {
                        command,
                        env,
                        binds,
                        hostname,
                        workdir,
                        net,
                        publish,
                        keep_rootfs,
                        from,
                    },
                )
            } else {
                commands::run(
                    &store_root,
                    &reference,
                    commands::RunArgs {
                        command,
                        env,
                        binds,
                        hostname,
                        workdir,
                        net,
                        publish,
                        join_net,
                        keep_rootfs,
                        run_dir,
                        map_user,
                    },
                )
            }
        }
        Command::Build {
            file,
            tag,
            timestamp,
        } => commands::build(&store_root, &file, tag.as_deref(), timestamp.as_deref()),
        Command::Ls { paths } => commands::ls(&store_root, paths),
        Command::Inspect { reference, files } => commands::inspect(&store_root, &reference, files),
        Command::Rm { reference } => commands::rm(&store_root, &reference),
        Command::Gc => commands::gc(&store_root),
        Command::Pin { reference } => commands::pin(&store_root, &reference, true),
        Command::Unpin { reference } => commands::pin(&store_root, &reference, false),
        Command::Stats => commands::stats(&store_root),
        Command::Export {
            reference,
            output_pos,
            output,
            oci,
        } => {
            let out = output.or(output_pos);
            if oci {
                commands::export_oci(&store_root, &reference, out.as_deref())
            } else {
                commands::export(&store_root, &reference, out.as_deref())
            }
        }
        Command::Import { archive } => commands::import(&store_root, &archive),
        Command::Up { file, net } => commands::up(&store_root, &file, net.as_deref()),
        Command::Down { file } => commands::down(&store_root, &file),
        Command::PodHolder { die_with_parent } => {
            std::process::exit(myc_run::pod::holder_main(die_with_parent))
        }
        Command::Verify { reference } => commands::verify(&store_root, &reference),
        Command::Diff { a, b } => commands::diff(&store_root, &a, &b),
        Command::Which { query } => commands::which(&store_root, &query),
        Command::Sbom { reference } => commands::sbom(&store_root, &reference),
        Command::Shell { image } => commands::shell(&store_root, &image),
        Command::Doctor => commands::doctor(&store_root),
        Command::Volume(VolumeCommand::Ls) => commands::volume_ls(&store_root),
        Command::Volume(VolumeCommand::Inspect { name }) => {
            commands::volume_inspect(&store_root, &name)
        }
        Command::Volume(VolumeCommand::Rm { name }) => commands::volume_rm(&store_root, &name),
        Command::Hub(HubCommand::Serve { port, token }) => {
            hub_commands::hub_serve(&store_root, port, token.as_deref())
        }
        Command::Push {
            reference,
            to,
            token,
        } => hub_commands::push(&store_root, &reference, to.as_deref(), token.as_deref()),
        Command::Ui { port } => commands::ui(&store_root, port),
        Command::Deploy {
            reference,
            target,
            run,
            restart,
            publish,
            binds,
            net,
            myc_path,
            no_bootstrap,
        } => deploy_commands::deploy(
            &store_root,
            &reference,
            &target,
            deploy_commands::DeployArgs {
                run,
                restart,
                publish,
                binds,
                net,
                myc_path,
                no_bootstrap,
            },
        ),
        Command::ServeDeploy => deploy_commands::serve_deploy(&store_root),
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("myc: error: {e:#}");
            for hint in hints_for(&format!("{e:#}")) {
                eprintln!("myc: hint: {hint}");
            }
            std::process::exit(1);
        }
    }
}

/// Turn well-known failure signatures into one-line, actionable hints.
fn hints_for(error: &str) -> Vec<&'static str> {
    let lower = error.to_lowercase();
    let mut hints = Vec::new();
    if lower.contains("operation not permitted")
        || lower.contains("user namespace")
        || lower.contains("unshare")
    {
        hints.push(
            "this usually means unprivileged user namespaces are disabled — run `myc doctor` for a diagnosis and the exact fix",
        );
    }
    if lower.contains("dns error")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("timed out")
    {
        hints.push(
            "looks like a network problem — `myc doctor` tests registry connectivity; already-ingested environments keep working offline",
        );
    }
    hints
}
