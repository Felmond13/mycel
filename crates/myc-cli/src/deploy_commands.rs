//! `myc deploy`: ship an environment to any Linux server over SSH, moving
//! only the blobs the server is missing, then optionally start it.
//!
//! No agent, no registry, no Docker on the target — just sshd and a `myc`
//! binary (uploaded automatically when absent). The wire protocol lives in
//! `myc_hub::deploy`, and the transport / bootstrap / launch machinery is
//! shared with the web dashboard in `myc_hub::remote` (the transport stays
//! injectable through `$MYCEL_SSH_CMD`; see that module). This module owns
//! only the CLI half: flags in, terminal output out.

use crate::ui::human_bytes;
use anyhow::{bail, Context, Result};
use myc_hub::remote::{
    ensure_remote_myc, launch_remote, run_deploy, BootstrapOptions, LaunchOptions, Target,
    Transport,
};
use myc_store::Store;
use std::io::Write;
use std::path::Path;

/// Everything `myc deploy` needs beyond the ref and the target.
pub struct DeployArgs {
    /// Start the environment on the target after the transfer.
    pub run: bool,
    /// Stop a previous instance of the same ref first (implies --run).
    pub restart: bool,
    /// `-p HOST:CONTAINER` specs forwarded to the remote `myc run`.
    pub publish: Vec<String>,
    /// `-v` mount specs forwarded to the remote `myc run`.
    pub binds: Vec<String>,
    /// `--net` mode forwarded to the remote `myc run`.
    pub net: Option<String>,
    /// Path of the `myc` binary on the target (default: `myc` in PATH,
    /// falling back to `~/.local/bin/myc`).
    pub myc_path: Option<String>,
    /// Never upload a `myc` binary to the target.
    pub no_bootstrap: bool,
}

/// `myc deploy REF TARGET`: negotiate, send the delta, register the ref —
/// then optionally launch it.
pub fn deploy(root: &Path, reference: &str, target_spec: &str, args: DeployArgs) -> Result<i32> {
    let store =
        Store::open(root).with_context(|| format!("cannot open store at {}", root.display()))?;
    let id = store
        .resolve(reference)
        .with_context(|| format!("'{reference}' not found in the local store"))?;
    let manifest = store.get_manifest(&id)?;
    let missing_local = store.missing_blobs(&manifest);
    if !missing_local.is_empty() {
        bail!(
            "{} blobs missing locally; run `myc pull {reference}` before deploying",
            missing_local.len()
        );
    }

    let target = Target::parse(target_spec)?;
    let transport = Transport::new(target);
    let myc = ensure_remote_myc(
        &transport,
        &BootstrapOptions {
            myc_path: args.myc_path.clone(),
            no_bootstrap: args.no_bootstrap,
        },
        &|msg| eprintln!("myc: {msg}"),
    )?;

    let progress = |done: usize, total: usize| {
        eprint!("\rmyc: sending blob {done}/{total}…");
        let _ = std::io::stderr().flush();
    };
    let report = run_deploy(&store, &manifest, reference, &transport, &myc, &progress)?;
    if report.blobs_sent > 0 {
        eprintln!();
    }

    println!(
        "deployed {reference} → {} — {} file{} sent ({}), {} already present — done in {:.1?}",
        transport.dest(),
        report.blobs_sent,
        if report.blobs_sent == 1 { "" } else { "s" },
        human_bytes(report.bytes_sent),
        report.blobs_total - report.blobs_sent,
        report.elapsed,
    );

    if args.run || args.restart {
        let launch = launch_remote(
            &transport,
            &myc,
            reference,
            &manifest,
            &LaunchOptions {
                restart: args.restart,
                publish: args.publish,
                binds: args.binds,
                net: args.net,
            },
        )?;
        println!(
            "started {reference} on {} (pid {}, log: ~/.mycel/deploys/{}.log){}",
            transport.dest(),
            launch.pid,
            launch.name,
            if launch.ports.is_empty() {
                String::new()
            } else {
                format!(
                    " — port{} {}",
                    if launch.ports.len() == 1 { "" } else { "s" },
                    launch.ports.join(", ")
                )
            },
        );
    }
    Ok(0)
}

/// `myc _serve-deploy` (hidden plumbing): the remote half of a deploy,
/// speaking the protocol on stdin/stdout. Invoked by `myc deploy` through
/// ssh — never by hand.
pub fn serve_deploy(root: &Path) -> Result<i32> {
    let store =
        Store::open(root).with_context(|| format!("cannot open store at {}", root.display()))?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    myc_hub::deploy::serve_deploy(&store, &mut stdin.lock(), &mut stdout.lock())?;
    Ok(0)
}
