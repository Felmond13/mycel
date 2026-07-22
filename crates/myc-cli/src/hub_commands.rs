//! Hub-related subcommands: `myc hub serve`, `myc push`, hub-side pulls and
//! `myc run --lazy`. Kept separate from commands.rs to stay small and
//! focused on the store-to-store transfer path.

use crate::ui::human_bytes;
#[cfg(target_os = "linux")]
use crate::ui::short_id;
use anyhow::{bail, Context, Result};
use myc_hub::{transfer, HubClient};
#[cfg(target_os = "linux")]
use myc_manifest::Manifest;
use myc_store::Store;
use std::io::Write;
use std::path::Path;

/// Default hub: `--to/--from` beats `$MYCEL_HUB`.
pub fn hub_url(explicit: Option<&str>) -> Result<String> {
    if let Some(u) = explicit {
        return Ok(u.to_string());
    }
    if let Ok(u) = std::env::var("MYCEL_HUB") {
        if !u.is_empty() {
            return Ok(u);
        }
    }
    bail!("no hub given — pass --to/--from URL or set $MYCEL_HUB (e.g. http://hub:9600)")
}

/// Write token: `--token` beats `$MYCEL_HUB_TOKEN`.
fn hub_token(explicit: Option<&str>) -> Option<String> {
    explicit.map(str::to_string).or_else(|| {
        std::env::var("MYCEL_HUB_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
    })
}

/// Candidate names to try on a hub: the reference as given, then its
/// canonical docker-style form (`alpine:3.20` → `docker.io/library/alpine:3.20`).
fn hub_candidates(reference: &str) -> Vec<String> {
    let mut out = vec![reference.to_string()];
    if let Ok(image) = myc_oci::ImageReference::parse(reference) {
        let canonical = image.canonical_name();
        if canonical != reference {
            out.push(canonical);
        }
    }
    out
}

/// `myc hub serve`: expose the store to the team over HTTP.
pub fn hub_serve(root: &Path, port: u16, token: Option<&str>) -> Result<i32> {
    let token = hub_token(token);
    if token.is_none() {
        eprintln!("myc: hub is open for writes (pass --token SECRET to require auth)");
    }
    myc_hub::serve(root.to_path_buf(), port, token)?;
    Ok(0)
}

/// `myc push REF [--to URL]`: manifest + only-the-missing blobs + ref.
pub fn push(root: &Path, reference: &str, to: Option<&str>, token: Option<&str>) -> Result<i32> {
    let store =
        Store::open(root).with_context(|| format!("cannot open store at {}", root.display()))?;
    let url = hub_url(to)?;
    let client = HubClient::new(&url, hub_token(token))?;

    let id = store
        .resolve(reference)
        .with_context(|| format!("'{reference}' not found in the local store"))?;
    let manifest = store.get_manifest(&id)?;
    let missing_local = store.missing_blobs(&manifest);
    if !missing_local.is_empty() {
        bail!(
            "{} blobs missing locally; run `myc pull {reference}` before pushing",
            missing_local.len()
        );
    }

    let report = transfer::push(&store, &client, &manifest, reference, &|done, total| {
        eprint!("\rmyc: uploading blob {done}/{total}…");
        let _ = std::io::stderr().flush();
    })?;
    if report.blobs_uploaded > 0 {
        eprintln!();
    }
    println!(
        "pushed {reference} — {} blobs of {} uploaded ({} of {}) in {:.1?}",
        report.blobs_uploaded,
        report.blobs_total,
        human_bytes(report.bytes_uploaded),
        human_bytes(report.bytes_total),
        report.elapsed,
    );
    Ok(0)
}

/// Hub side of `myc pull REF [--from URL]`.
pub fn pull_from_hub(root: &Path, reference: &str, from: Option<&str>) -> Result<i32> {
    let store =
        Store::open(root).with_context(|| format!("cannot open store at {}", root.display()))?;
    let url = hub_url(from)?;
    let client = HubClient::new(&url, None)?;

    let progress = |done: usize, total: usize| {
        eprint!("\rmyc: downloading blob {done}/{total}…");
        let _ = std::io::stderr().flush();
    };
    let mut candidates = hub_candidates(reference).into_iter().peekable();
    let report = loop {
        let name = candidates.next().expect("at least one candidate");
        match transfer::pull(&store, &client, &name, &progress) {
            Ok(r) => break r,
            // Short name not on the hub: try the canonical docker-style one.
            Err(myc_hub::client::HubError::NotFound { .. }) if candidates.peek().is_some() => {}
            Err(e) => return Err(e.into()),
        }
    };
    // Make the name the user typed resolvable locally next time.
    if store.resolve(reference).is_err() {
        let _ = store.set_ref(reference, &report.manifest_id);
    }
    if report.blobs_downloaded > 0 {
        eprintln!();
    }
    println!(
        "pulled {reference} from {url} — {} blobs of {} downloaded ({} of {}) in {:.1?} — offline-ready",
        report.blobs_downloaded,
        report.blobs_total,
        human_bytes(report.bytes_downloaded),
        human_bytes(report.bytes_total),
        report.elapsed,
    );
    Ok(0)
}

/// Resolve `reference` for a lazy run: local store first, then the hub
/// (manifest only — that is the whole point).
#[cfg(target_os = "linux")]
fn resolve_lazy(store: &Store, client: &HubClient, reference: &str) -> Result<(String, Manifest)> {
    if let Ok(id) = store.resolve(reference) {
        let manifest = store.get_manifest(&id)?;
        return Ok((id, manifest));
    }
    eprintln!(
        "myc: '{reference}' not local, fetching manifest from {}…",
        client.base_url()
    );
    let mut candidates = hub_candidates(reference).into_iter().peekable();
    let (id, manifest) = loop {
        let name = candidates.next().expect("at least one candidate");
        match transfer::pull_manifest_only(store, client, &name) {
            Ok(found) => break found,
            Err(myc_hub::client::HubError::NotFound { .. }) if candidates.peek().is_some() => {}
            Err(e) => return Err(e.into()),
        }
    };
    if store.resolve(reference).is_err() {
        let _ = store.set_ref(reference, &id);
    }
    Ok((id, manifest))
}

// On non-Linux hosts the lazy path is a stub, so the fields go unread there.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct LazyRunArgs {
    pub command: Vec<String>,
    pub env: Vec<String>,
    pub binds: Vec<String>,
    pub hostname: String,
    pub workdir: Option<String>,
    /// Network mode (`host`, `isolated` or `none`) — see `commands::build_network`.
    pub net: String,
    /// `-p HOST:CONTAINER` publish specs (isolated mode only).
    pub publish: Vec<String>,
    pub keep_rootfs: bool,
    pub from: Option<String>,
}

/// `myc run --lazy REF`: start before the blobs are local; stream the rest
/// on first access, caching every fetched file in the store forever.
#[cfg(target_os = "linux")]
pub fn run_lazy(root: &Path, reference: &str, args: LazyRunArgs) -> Result<i32> {
    let start = std::time::Instant::now();
    let store =
        Store::open(root).with_context(|| format!("cannot open store at {}", root.display()))?;
    let url = hub_url(args.from.as_deref())?;
    let client = HubClient::new(&url, None)?;
    let (id, manifest) = resolve_lazy(&store, &client, reference)?;

    let cwd = std::env::current_dir()?;
    let binds = crate::commands::resolve_mounts(&store, &args.binds, &cwd, Some(reference))?;
    let options = myc_run::RunOptions {
        command: args.command,
        env: args.env,
        hostname: args.hostname,
        binds,
        workdir: args.workdir,
        keep_rootfs: args.keep_rootfs,
        network: crate::commands::build_network(&args.net, &args.publish)?,
        ..Default::default()
    };

    let run_dir = tempfile::Builder::new()
        .prefix(&format!("lazy-{}-", &short_id(&id)["myc1-".len()..]))
        .tempdir_in(store.root().join("tmp"))
        .context("cannot create run directory")?;
    let rootfs = run_dir.path().join("rootfs");

    let lazy = myc_lazy::LazyRootfs::prepare(&store, root, &manifest, &url, None, &rootfs)?;
    let p = lazy.prepare_stats;
    eprintln!(
        "myc: lazy rootfs ready in {:.0?} — {} files local, {} deferred ({}) to {url}",
        start.elapsed(),
        p.local_files,
        p.deferred_files,
        human_bytes(p.deferred_bytes),
    );

    let code = myc_run::exec_prepared(&manifest, &rootfs, &options)?;

    let s = lazy.run_stats();
    eprintln!(
        "myc: lazy run done — fetched {} of {} deferred files on demand ({}), {} open(s) hit the local cache{}",
        s.fetched_blobs,
        p.deferred_files,
        human_bytes(s.fetched_bytes),
        s.local_hits,
        if s.errors > 0 {
            format!(", {} fetch error(s)", s.errors)
        } else {
            String::new()
        },
    );

    if args.keep_rootfs {
        drop(lazy); // unmount, keep files
        let kept = run_dir.keep();
        eprintln!("rootfs kept at {}", kept.display());
    } else {
        lazy.cleanup().ok(); // unmount + delete before the tempdir goes
    }
    Ok(code)
}

/// Lazy streaming needs FUSE, which is Linux-only. On Windows the default
/// proxy sends `myc run --lazy` into WSL2, so this stub is reachable only
/// with `--no-proxy` (or on other non-Linux hosts).
#[cfg(not(target_os = "linux"))]
pub fn run_lazy(_root: &Path, _reference: &str, _args: LazyRunArgs) -> Result<i32> {
    bail!(
        "`myc run --lazy` needs Linux (FUSE) — on Windows, run it without --no-proxy \
         so it executes inside WSL2"
    )
}
