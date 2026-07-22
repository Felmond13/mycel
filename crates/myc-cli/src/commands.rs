//! Implementation of every `myc` subcommand.

use crate::ui::{human_bytes, short_id, Paint};
use anyhow::{bail, Context, Result};
use myc_manifest::Manifest;
use myc_oci::{IngestProgress, Platform};
use myc_store::Store;
use std::io::Write;
use std::path::{Path, PathBuf};

fn open_store(root: &Path) -> Result<Store> {
    Store::open(root).with_context(|| format!("cannot open store at {}", root.display()))
}

/// Resolve a reference, ingesting from the registry when unknown locally.
fn resolve_or_ingest(store: &Store, root: &Path, reference: &str) -> Result<(String, Manifest)> {
    if let Ok(id) = store.resolve(reference) {
        let manifest = store.get_manifest(&id)?;
        return Ok((id, manifest));
    }
    // The short form may be stored under its canonical name
    // (e.g. `alpine:3.20` -> `docker.io/library/alpine:3.20`).
    if let Ok(image_ref) = myc_oci::ImageReference::parse(reference) {
        if let Ok(id) = store.resolve(&image_ref.canonical_name()) {
            store.set_ref(reference, &id)?;
            let manifest = store.get_manifest(&id)?;
            return Ok((id, manifest));
        }
    }
    // Not local. If it cannot even be an image reference, fail with
    // suggestions right away; otherwise try the registry.
    if myc_oci::ImageReference::parse(reference).is_err() {
        bail!("{}", not_found_message(store, reference));
    }
    eprintln!("myc: '{reference}' not in local store, ingesting from registry…");
    let report = match ingest_to_store(store, reference, None) {
        Ok(report) => report,
        Err(e) => bail!(
            "'{reference}' is not in the local store, and fetching it from the registry failed:\n  {e}\n{}",
            not_found_message(store, reference)
        ),
    };
    eprintln!(
        "myc: ingested {} ({} files, {})",
        short_id(&report.manifest_id),
        report.file_count,
        human_bytes(report.logical_size)
    );
    let _ = root;
    let id = store.resolve(reference)?;
    let manifest = store.get_manifest(&id)?;
    Ok((id, manifest))
}

/// Resolve a reference that must already be local, failing with suggestions
/// instead of a raw store error.
fn resolve_local(store: &Store, reference: &str) -> Result<String> {
    store
        .resolve(reference)
        .map_err(|_| anyhow::anyhow!("{}", not_found_message(store, reference)))
}

/// Friendly "not found" text with `did you mean` suggestions drawn from the
/// names already in the store.
fn not_found_message(store: &Store, reference: &str) -> String {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(refs) = store.list_refs() {
        candidates.extend(refs.into_iter().map(|(name, _)| name));
    }
    let mut msg = format!("'{reference}' not found in the local store");
    let suggestions = crate::ui::suggest(reference, &candidates);
    if !suggestions.is_empty() {
        msg.push_str(&format!("\n  did you mean: {}?", suggestions.join(", ")));
    }
    msg.push_str(
        "\n  hint: `myc ls` lists local environments; `myc ingest NAME:TAG` fetches new ones",
    );
    msg
}

fn ingest_to_store(
    store: &Store,
    image: &str,
    arch: Option<&str>,
) -> Result<myc_oci::IngestReport> {
    let mut platform = Platform::host();
    if let Some(a) = arch {
        platform.arch = a.to_string();
    }
    eprintln!("Ingesting {image} ({}/{})…", platform.os, platform.arch);
    Ok(myc_oci::ingest(store, image, &platform, &mut CliProgress)?)
}

struct CliProgress;
impl IngestProgress for CliProgress {
    fn layer_started(&mut self, index: usize, total: usize, digest: &str) {
        let short = digest.strip_prefix("sha256:").unwrap_or(digest);
        eprint!(
            "  layer {}/{} {}… ",
            index + 1,
            total,
            &short[..12.min(short.len())]
        );
        let _ = std::io::stderr().flush();
    }
    fn layer_done(&mut self, _index: usize, files_added: usize, bytes_added: u64) {
        eprintln!("{files_added} files, {} new", human_bytes(bytes_added));
    }
}

pub fn ingest(root: &Path, image: &str, arch: Option<&str>) -> Result<i32> {
    let store = open_store(root)?;
    let report = ingest_to_store(&store, image, arch)?;
    println!(
        "{}  {}\n  {} files, {} logical, {} added to store (dedup saved {})",
        short_id(&report.manifest_id),
        report.name,
        report.file_count,
        human_bytes(report.logical_size),
        human_bytes(report.bytes_added),
        human_bytes(report.logical_size.saturating_sub(report.bytes_added)),
    );
    Ok(0)
}

pub fn pull(root: &Path, reference: &str) -> Result<i32> {
    let store = open_store(root)?;
    let (id, manifest) = resolve_or_ingest(&store, root, reference)?;
    let missing = store.missing_blobs(&manifest);
    if missing.is_empty() {
        println!(
            "{} complete: all {} blobs present — offline-ready",
            short_id(&id),
            manifest.referenced_blobs().len()
        );
        Ok(0)
    } else {
        // Blobs can only be missing if a manifest was imported without its
        // blobs; re-ingest from origin to fill the gaps.
        let origin = manifest
            .origin
            .as_deref()
            .and_then(|o| o.strip_prefix("oci://"))
            .map(str::to_string);
        match origin {
            Some(image) => {
                eprintln!("{} blobs missing, re-fetching from {image}…", missing.len());
                ingest(root, &image, Some(&manifest.arch))?;
                let still = store.missing_blobs(&manifest);
                if still.is_empty() {
                    println!("{} complete — offline-ready", short_id(&id));
                    Ok(0)
                } else {
                    bail!("{} blobs still missing after re-fetch", still.len());
                }
            }
            None => bail!(
                "{} blobs missing and manifest has no origin to fetch from",
                missing.len()
            ),
        }
    }
}

/// `myc build`: native image build from a mycel-build.toml.
pub fn build(root: &Path, file: &Path, tag: Option<&str>, timestamp: Option<&str>) -> Result<i32> {
    let store = open_store(root)?;
    let (spec, context_dir) =
        myc_build::BuildSpec::load(file).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Resolve the base through the store, auto-ingesting from the registry.
    let base = match spec.build.base.clone() {
        Some(base_ref) => Some(
            resolve_or_ingest(&store, root, &base_ref)
                .with_context(|| format!("cannot resolve base image '{base_ref}'"))?
                .1,
        ),
        None => None,
    };

    // Scratch space for run-step rootfs materialization.
    let work_dir = tempfile::Builder::new()
        .prefix("build-")
        .tempdir_in(store.root().join("tmp"))
        .context("cannot create build directory")?;

    let request = myc_build::BuildRequest {
        spec: &spec,
        context_dir: &context_dir,
        base: base.as_ref(),
        name_override: tag,
        timestamp,
        origin: Some(format!("build://{}", file.display())),
        work_dir: work_dir.path(),
    };
    let report = myc_build::build(&store, &request, &mut CliBuildProgress)?;
    println!(
        "{}  {}\n  {} files, {} logical, {} added to store (dedup saved {})",
        short_id(&report.manifest_id),
        report.name,
        report.file_count,
        human_bytes(report.logical_size),
        human_bytes(report.bytes_added),
        human_bytes(report.logical_size.saturating_sub(report.bytes_added)),
    );
    Ok(0)
}

struct CliBuildProgress;
impl myc_build::BuildProgress for CliBuildProgress {
    fn add_done(&mut self, source: &str, dest: &str, stats: &myc_build::add::AddStats) {
        eprintln!(
            "  add {source} -> {dest}: {} file(s), {} new",
            stats.files,
            human_bytes(stats.bytes_added)
        );
    }
    fn step_started(&mut self, index: usize, total: usize, command: &[String]) {
        eprintln!("  step {}/{}: {}", index + 1, total, command.join(" "));
    }
    fn step_done(&mut self, index: usize, s: &myc_build::StepSummary) {
        eprintln!(
            "  step {} ok: +{} added, ~{} modified, -{} removed, {} to store, {:.1}s",
            index + 1,
            s.added,
            s.modified,
            s.removed,
            human_bytes(s.bytes_added),
            s.duration.as_secs_f64()
        );
    }
}

pub struct RunArgs {
    pub command: Vec<String>,
    pub env: Vec<String>,
    pub binds: Vec<String>,
    pub hostname: String,
    pub workdir: Option<String>,
    /// Network mode: `host` (default), `isolated` (own namespace, pasta
    /// connectivity, published ports) or `none` (own namespace, no
    /// connectivity at all).
    pub net: String,
    /// `-p HOST:CONTAINER` publish specs (isolated mode only).
    pub publish: Vec<String>,
    pub keep_rootfs: bool,
    /// Materialize into this caller-owned directory instead of a fresh temp
    /// dir. Used by the web UI, which cleans the directory up itself even
    /// when the run process is killed.
    pub run_dir: Option<PathBuf>,
    /// `UID[:GID]` to map the host user to inside the container (default
    /// root). Official images (postgres, nginx-unprivileged, …) detect a
    /// non-root uid and skip their chown/su-exec paths, which cannot work
    /// in a single-uid user namespace.
    pub map_user: Option<String>,
}

impl Default for RunArgs {
    fn default() -> Self {
        RunArgs {
            command: Vec::new(),
            env: Vec::new(),
            binds: Vec::new(),
            hostname: "mycel".to_string(),
            workdir: None,
            net: "host".to_string(),
            publish: Vec::new(),
            keep_rootfs: false,
            run_dir: None,
            map_user: None,
        }
    }
}

/// Parse a `-p` publish spec: `HOST:CONTAINER` (e.g. `6380:6379`) or a
/// single `PORT` published on the same host port.
pub fn parse_publish(spec: &str) -> Result<myc_run::PortMap> {
    let port = |s: &str| -> Result<u16> {
        s.parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .with_context(|| format!("invalid port '{s}' in '-p {spec}' (expected 1-65535)"))
    };
    let (host, container) = match spec.split_once(':') {
        Some((h, c)) => (port(h)?, port(c)?),
        None => {
            let p = port(spec)?;
            (p, p)
        }
    };
    Ok(myc_run::PortMap { host, container })
}

/// Combine `--net` and `-p` into the runtime network mode. `-p` implies
/// isolated mode (there is nothing to publish on the host network).
pub fn build_network(net: &str, publish: &[String]) -> Result<myc_run::Network> {
    let ports: Vec<myc_run::PortMap> = publish
        .iter()
        .map(|s| parse_publish(s))
        .collect::<Result<_>>()?;
    match net {
        "isolated" => Ok(myc_run::Network::Isolated { publish: ports }),
        "host" if !ports.is_empty() => Ok(myc_run::Network::Isolated { publish: ports }),
        "host" => Ok(myc_run::Network::Host),
        "none" if ports.is_empty() => Ok(myc_run::Network::Loopback),
        "none" => bail!("-p cannot be used with --net none (no connectivity to publish through)"),
        other => bail!("invalid --net '{other}' (expected host, isolated or none)"),
    }
}

/// Parse `UID[:GID]` (e.g. `999` or `101:101`).
fn parse_map_user(spec: &str) -> Result<(u32, u32)> {
    let (uid, gid) = match spec.split_once(':') {
        Some((u, g)) => (u, g),
        None => (spec, spec),
    };
    let uid: u32 = uid
        .parse()
        .with_context(|| format!("invalid --map-user '{spec}' (expected UID[:GID])"))?;
    let gid: u32 = gid
        .parse()
        .with_context(|| format!("invalid --map-user '{spec}' (expected UID[:GID])"))?;
    Ok((uid, gid))
}

/// Turn `-v` specs into concrete (host, container, ro) mounts. A source
/// that is not path-like (`myvol:/data`) is a named volume, created under
/// the store on first use — exactly like Docker's named volumes.
pub fn resolve_mounts(
    store: &Store,
    specs: &[String],
    base: &Path,
    created_for: Option<&str>,
) -> Result<Vec<(PathBuf, String, bool)>> {
    let mut mounts = Vec::new();
    for spec in specs {
        match myc_compose::parse_mount(spec, base).map_err(|e| anyhow::anyhow!("{e}"))? {
            myc_compose::Mount::Bind {
                host,
                container,
                read_only,
            } => {
                if !host.exists() {
                    bail!("bind source does not exist: {}", host.display());
                }
                mounts.push((host, container, read_only));
            }
            myc_compose::Mount::Volume {
                name,
                container,
                read_only,
            } => {
                let data = store.ensure_volume(&name, created_for)?;
                mounts.push((data, container, read_only));
            }
        }
    }
    Ok(mounts)
}

pub fn run(root: &Path, reference: &str, args: RunArgs) -> Result<i32> {
    let store = open_store(root)?;
    let (id, manifest) = resolve_or_ingest(&store, root, reference)?;

    let cwd = std::env::current_dir()?;
    let binds = resolve_mounts(&store, &args.binds, &cwd, Some(reference))?;

    let (map_uid, map_gid) = match &args.map_user {
        Some(spec) => parse_map_user(spec)?,
        None => (0, 0),
    };
    let options = myc_run::RunOptions {
        command: args.command,
        env: args.env,
        hostname: args.hostname,
        binds,
        workdir: args.workdir,
        keep_rootfs: args.keep_rootfs,
        network: build_network(&args.net, &args.publish)?,
        map_uid,
        map_gid,
    };

    // A caller-owned run dir skips temp-dir management entirely: the caller
    // (the web UI) deletes it after the process exits, which stays correct
    // even if this process is SIGKILLed mid-run.
    if let Some(dir) = &args.run_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create run directory {}", dir.display()))?;
        return Ok(myc_run::run(&store, &manifest, dir, &options)?);
    }

    // Each run gets a fresh working dir under the store.
    let run_dir = tempfile::Builder::new()
        .prefix(&format!("run-{}-", &short_id(&id)["myc1-".len()..]))
        .tempdir_in(store.root().join("tmp"))
        .context("cannot create run directory")?;

    let code = myc_run::run(&store, &manifest, run_dir.path(), &options)?;
    if options.keep_rootfs {
        let kept = run_dir.keep();
        eprintln!("rootfs kept at {}", kept.display());
    }
    Ok(code)
}

pub fn ls(root: &Path, paths: bool) -> Result<i32> {
    let store = open_store(root)?;
    let manifests = store.list_manifests()?;
    let pins = store.pins()?;
    if manifests.is_empty() {
        println!("store is empty — try `myc ingest alpine:3.20`");
        return Ok(0);
    }
    println!(
        "{:<19} {:<45} {:>7} {:>10}  PINNED",
        "ID", "NAME", "FILES", "SIZE"
    );
    for (id, m) in manifests {
        println!(
            "{:<19} {:<45} {:>7} {:>10}  {}",
            short_id(&id),
            m.name.chars().take(45).collect::<String>(),
            m.file_count(),
            human_bytes(m.logical_size()),
            if pins.contains(&id) { "yes" } else { "" }
        );
        if paths {
            // Bridge name -> full id -> manifest JSON on disk.
            let json = store.root().join("manifests").join(format!("{id}.json"));
            println!("  id:   {id}");
            println!("  path: {}", json.display());
        }
    }
    Ok(0)
}

pub fn inspect(root: &Path, reference: &str, files: bool) -> Result<i32> {
    let store = open_store(root)?;
    let id = resolve_local(&store, reference)?;
    let m = store.get_manifest(&id)?;
    println!("id:       {id}");
    println!("name:     {}", m.name);
    if let Some(origin) = &m.origin {
        println!("origin:   {origin}");
    }
    println!("platform: {}/{}", m.os, m.arch);
    println!("created:  {}", m.created);
    println!(
        "files:    {} ({} logical)",
        m.file_count(),
        human_bytes(m.logical_size())
    );
    let missing = store.missing_blobs(&m);
    println!(
        "blobs:    {} referenced, {} missing{}",
        m.referenced_blobs().len(),
        missing.len(),
        if missing.is_empty() {
            " (offline-ready)"
        } else {
            ""
        }
    );
    if !m.config.entrypoint.is_empty() {
        println!("entrypoint: {:?}", m.config.entrypoint);
    }
    if !m.config.cmd.is_empty() {
        println!("cmd:      {:?}", m.config.cmd);
    }
    if !m.config.workdir.is_empty() {
        println!("workdir:  {}", m.config.workdir);
    }
    for e in &m.config.env {
        println!("env:      {e}");
    }
    if files {
        println!();
        for e in &m.entries {
            let kind = match e.kind {
                myc_manifest::EntryKind::File => "f",
                myc_manifest::EntryKind::Dir => "d",
                myc_manifest::EntryKind::Symlink => "l",
                _ => "?",
            };
            match &e.target {
                Some(t) => println!("{kind} {:o} {:>10} {} -> {t}", e.mode, e.size, e.path),
                None => println!("{kind} {:o} {:>10} {}", e.mode, e.size, e.path),
            }
        }
    }
    Ok(0)
}

pub fn rm(root: &Path, reference: &str) -> Result<i32> {
    let store = open_store(root)?;
    let id = resolve_local(&store, reference)?;
    store.remove_manifest(&id)?;
    println!(
        "removed {} (run `myc gc` to reclaim blob space)",
        short_id(&id)
    );
    Ok(0)
}

pub fn gc(root: &Path) -> Result<i32> {
    let store = open_store(root)?;
    // Pinned manifests are already in the manifest list, so their blobs are
    // live by definition; pins additionally protect the manifest from `rm`.
    let (deleted, freed) = store.gc()?;
    println!("gc: {deleted} blobs deleted, {} freed", human_bytes(freed));
    Ok(0)
}

pub fn pin(root: &Path, reference: &str, add: bool) -> Result<i32> {
    let store = open_store(root)?;
    let id = resolve_local(&store, reference)?;
    if add {
        store.pin(&id)?;
        println!("pinned {}", short_id(&id));
    } else {
        store.unpin(&id)?;
        println!("unpinned {}", short_id(&id));
    }
    Ok(0)
}

pub fn stats(root: &Path) -> Result<i32> {
    let store = open_store(root)?;
    let paint = Paint::auto();
    let manifests = store.list_manifests()?;
    let blob_stats = store.blob_stats()?;
    let logical: u64 = manifests.iter().map(|(_, m)| m.logical_size()).sum();
    println!("environments:  {}", manifests.len());
    println!(
        "blobs:         {} ({} physical)",
        blob_stats.count,
        human_bytes(blob_stats.bytes)
    );
    println!("logical size:  {}", human_bytes(logical));
    if blob_stats.bytes > 0 && logical > 0 {
        let ratio = logical as f64 / blob_stats.bytes as f64;
        println!(
            "dedup ratio:   {} ({} saved)",
            paint.bold(&format!("{ratio:.2}x")),
            human_bytes(logical.saturating_sub(blob_stats.bytes))
        );
    }

    // Per-environment sharing: how much of each env is already covered by
    // the others — the content-addressed superpower, quantified.
    if manifests.len() > 1 {
        let report = myc_query::dedup_report(&manifests);
        println!();
        println!(
            "{}",
            paint.bold(&format!(
                "{:<45} {:>7} {:>8} {:>8} {:>12}",
                "PER-ENVIRONMENT SHARING", "BLOBS", "SHARED", "UNIQUE", "UNIQUE SIZE"
            ))
        );
        for d in report {
            // Pad before painting: ANSI escapes must not count toward width.
            let pct = format!("{:>8}", format!("{:.0}%", d.shared_ratio * 100.0));
            println!(
                "{:<45} {:>7} {} {:>8} {:>12}",
                d.name.chars().take(45).collect::<String>(),
                d.blobs,
                paint.green(&pct),
                d.unique_blobs,
                human_bytes(d.unique_bytes),
            );
        }
    }
    Ok(0)
}

pub fn export(root: &Path, reference: &str, output: Option<&Path>) -> Result<i32> {
    let store = open_store(root)?;
    let id = resolve_local(&store, reference)?;
    let manifest = store.get_manifest(&id)?;
    let missing = store.missing_blobs(&manifest);
    if !missing.is_empty() {
        bail!(
            "{} blobs missing; run `myc pull {reference}` first",
            missing.len()
        );
    }

    let default_name = format!("{}.mycel.tar.gz", manifest.name.replace(['/', ':'], "_"));
    let out_path = output
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default_name));
    let file = std::fs::File::create(&out_path)
        .with_context(|| format!("cannot create {}", out_path.display()))?;
    let report = myc_store::archive::export_archive(&store, &manifest, file)?;
    println!(
        "exported {} ({} blobs, {}) -> {}",
        short_id(&id),
        report.blobs,
        human_bytes(report.bytes),
        out_path.display()
    );
    Ok(0)
}

/// `myc export --oci`: write a standard OCI image tar — loadable by
/// `docker load`, podman, skopeo, crane, Kubernetes. The escape hatch that
/// makes Mycel fully reversible.
pub fn export_oci(root: &Path, reference: &str, output: Option<&Path>) -> Result<i32> {
    let store = open_store(root)?;
    let id = resolve_local(&store, reference)?;
    let manifest = store.get_manifest(&id)?;
    let missing = store.missing_blobs(&manifest);
    if !missing.is_empty() {
        bail!(
            "{} blobs missing; run `myc pull {reference}` first",
            missing.len()
        );
    }

    let default_name = format!("{}.oci.tar", manifest.name.replace(['/', ':'], "_"));
    let out_path = output
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default_name));
    let file = std::fs::File::create(&out_path)
        .with_context(|| format!("cannot create {}", out_path.display()))?;
    let report = myc_oci::export_oci_image(&store, &manifest, std::io::BufWriter::new(file))?;
    println!(
        "exported {} as OCI image -> {}\n  {} ({} files, layer {} {})\n  load it anywhere: docker load < {}",
        short_id(&id),
        out_path.display(),
        report.repo_tag,
        report.files,
        human_bytes(report.layer_size),
        &report.layer_digest[..19.min(report.layer_digest.len())],
        out_path.display()
    );
    Ok(0)
}

pub fn import(root: &Path, archive: &Path) -> Result<i32> {
    let store = open_store(root)?;
    let file = std::fs::File::open(archive)
        .with_context(|| format!("cannot open {}", archive.display()))?;
    let report = myc_store::archive::import_archive(&store, file)?;
    println!(
        "imported {} {} ({} blobs) — offline-ready",
        short_id(&report.manifest_id),
        report.name,
        report.blobs
    );
    Ok(0)
}

pub fn verify(root: &Path, reference: &str) -> Result<i32> {
    let store = open_store(root)?;
    let id = resolve_local(&store, reference)?;
    let manifest = store.get_manifest(&id)?;
    let blobs = manifest.referenced_blobs();
    let total = blobs.len();
    let mut bad = 0;
    for (i, (hash, _)) in blobs.iter().enumerate() {
        eprint!("\rverifying blob {}/{total}…", i + 1);
        if let Err(e) = store.verify_blob(hash) {
            eprintln!("\ncorrupt: {e}");
            bad += 1;
        }
    }
    eprintln!();
    if bad == 0 {
        println!("{}: all {total} blobs verified", short_id(&id));
        Ok(0)
    } else {
        bail!("{bad}/{total} blobs corrupt");
    }
}

pub fn up(root: &Path, file: &Path) -> Result<i32> {
    let store = open_store(root)?;
    let project = myc_compose::ProjectFile::load(file)?;
    let base = file.parent().unwrap_or(Path::new(".")).to_path_buf();
    let order = project.start_order()?;
    let project_name = if project.project.name.is_empty() {
        "mycel".to_string()
    } else {
        project.project.name.clone()
    };

    // Resolve every image first (ingest what's missing) so startup is clean.
    for name in &order {
        let svc = &project.services[name];
        resolve_or_ingest(&store, root, &svc.image)
            .with_context(|| format!("service '{name}': cannot resolve image '{}'", svc.image))?;
    }

    eprintln!("Starting {} service(s): {}", order.len(), order.join(", "));

    // One thread per service. Containers inherit our stdio; SIGINT reaches
    // the whole foreground process group, stopping everything at once.
    let mut handles = Vec::new();
    for name in &order {
        let svc = &project.services[name];
        let store_root = root.to_path_buf();
        let image = svc.image.clone();
        let service_name = name.clone();
        let args = RunArgs {
            command: svc.command.clone(),
            env: svc.env.clone(),
            binds: svc
                .binds
                .iter()
                .map(|b| resolve_bind_to_spec(b, &base))
                .collect::<Result<Vec<_>>>()?,
            hostname: svc
                .hostname
                .clone()
                .unwrap_or_else(|| format!("{project_name}-{name}")),
            workdir: svc.workdir.clone(),
            net: "host".to_string(),
            publish: Vec::new(),
            keep_rootfs: false,
            run_dir: None,
            map_user: None,
        };
        handles.push(std::thread::spawn(move || {
            let code = match run(&store_root, &image, args) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[{service_name}] error: {e:#}");
                    return 1;
                }
            };
            eprintln!("[{service_name}] exited with code {code}");
            code
        }));
    }

    let mut worst = 0;
    for h in handles {
        let code = h.join().unwrap_or(1);
        if code != 0 {
            worst = code;
        }
    }
    Ok(worst)
}

/// `myc diff A B`: exact file-by-file comparison of two environments.
pub fn diff(root: &Path, ref_a: &str, ref_b: &str) -> Result<i32> {
    let store = open_store(root)?;
    let paint = Paint::auto();
    let (_, manifest_a) = resolve_or_ingest(&store, root, ref_a)?;
    let (_, manifest_b) = resolve_or_ingest(&store, root, ref_b)?;
    let report = myc_query::diff(&manifest_a, &manifest_b);

    for change in &report.changes {
        use myc_query::ChangeKind;
        let line = match change.change {
            ChangeKind::Added => paint.green(&format!(
                "+ {:<60} {:>10}",
                change.path,
                human_bytes(change.size_b)
            )),
            ChangeKind::Removed => paint.red(&format!(
                "- {:<60} {:>10}",
                change.path,
                human_bytes(change.size_a)
            )),
            ChangeKind::Modified => paint.yellow(&format!(
                "~ {:<60} {:>10}  {}",
                change.path,
                format!(
                    "{} -> {}",
                    human_bytes(change.size_a),
                    human_bytes(change.size_b)
                ),
                change.detail
            )),
            ChangeKind::MetaChanged => {
                paint.cyan(&format!("≈ {:<60} {}", change.path, change.detail))
            }
        };
        println!("{line}");
    }

    let total = report.total_changes();
    if total == 0 {
        println!(
            "{ref_a} and {ref_b} are identical ({} files)",
            report.unchanged
        );
    } else {
        println!();
        println!(
            "{} differs from {} by {} file{}: {} added, {} removed, {} modified, {} metadata-only — {} of new content ({} files unchanged)",
            paint.bold(ref_b),
            paint.bold(ref_a),
            total,
            if total == 1 { "" } else { "s" },
            paint.green(&report.added.to_string()),
            paint.red(&report.removed.to_string()),
            paint.yellow(&report.modified.to_string()),
            report.meta_changed,
            paint.bold(&human_bytes(report.new_bytes)),
            report.unchanged,
        );
    }
    Ok(0)
}

/// `myc which QUERY`: reverse lookup across every stored environment, by
/// content-hash prefix or path substring.
pub fn which(root: &Path, query: &str) -> Result<i32> {
    let store = open_store(root)?;
    let paint = Paint::auto();
    let manifests = store.list_manifests()?;
    let hits = myc_query::which(&manifests, query);
    if hits.is_empty() {
        println!("no environment contains '{query}'");
        println!(
            "  (searched {} environments by content hash and path)",
            manifests.len()
        );
        return Ok(1);
    }

    // Group hits per environment for readable output.
    let mut current: Option<&str> = None;
    for hit in &hits {
        if current != Some(hit.manifest_id.as_str()) {
            current = Some(hit.manifest_id.as_str());
            println!(
                "{}  {}",
                paint.bold(&hit.manifest_name),
                paint.dim(&short_id(&hit.manifest_id))
            );
        }
        let hash = hit
            .blake3
            .as_deref()
            .map(|h| h.chars().take(12).collect::<String>())
            .unwrap_or_default();
        println!(
            "  {} {:<60} {:>10}  {}",
            if hit.matched == myc_query::MatchBy::Hash {
                paint.cyan("hash")
            } else {
                paint.dim("path")
            },
            hit.path,
            human_bytes(hit.size),
            paint.dim(&hash),
        );
    }
    let envs: std::collections::BTreeSet<&str> =
        hits.iter().map(|h| h.manifest_id.as_str()).collect();
    println!();
    println!(
        "{} match{} across {} environment{}",
        hits.len(),
        if hits.len() == 1 { "" } else { "es" },
        envs.len(),
        if envs.len() == 1 { "" } else { "s" },
    );
    Ok(0)
}

/// `myc sbom REF`: machine-readable SBOM on stdout.
pub fn sbom(root: &Path, reference: &str) -> Result<i32> {
    let store = open_store(root)?;
    let (id, manifest) = resolve_or_ingest(&store, root, reference)?;
    let sbom = myc_query::sbom(&store, &id, &manifest);
    println!("{}", serde_json::to_string_pretty(&sbom)?);
    if let Some(source) = &sbom.package_source {
        eprintln!(
            "myc: {} packages from the {source} database, {} files total",
            sbom.packages.len(),
            sbom.file_count
        );
    } else {
        eprintln!(
            "myc: no package database found; SBOM lists all {} files with content hashes",
            sbom.file_count
        );
    }
    Ok(0)
}

/// `myc shell IMAGE`: one word to an interactive shell in any image.
pub fn shell(root: &Path, image: &str) -> Result<i32> {
    eprintln!("myc: starting /bin/sh in {image} — type `exit` or Ctrl-D to leave");
    run(
        root,
        image,
        RunArgs {
            command: vec!["/bin/sh".to_string()],
            ..RunArgs::default()
        },
    )
}

/// `myc doctor`: environment health checks with actionable fixes.
pub fn doctor(root: &Path) -> Result<i32> {
    let paint = Paint::auto();
    let checks = myc_query::run_checks(root);
    let mut failures = 0;
    for check in &checks {
        let verdict = if check.ok {
            paint.green("PASS")
        } else {
            failures += 1;
            paint.red("FAIL")
        };
        println!("{verdict}  {:<18} {}", check.name, check.detail);
        if let Some(fix) = &check.fix {
            println!("      {}", paint.yellow(&format!("fix: {fix}")));
        }
    }
    println!();
    if failures == 0 {
        println!(
            "{}",
            paint.green("all checks passed — this machine can ingest and run environments")
        );
        Ok(0)
    } else {
        println!(
            "{}",
            paint.red(&format!("{failures} check(s) failed — see fixes above"))
        );
        Ok(1)
    }
}

/// `myc volume ls`: every named volume with its size.
pub fn volume_ls(root: &Path) -> Result<i32> {
    let store = open_store(root)?;
    let volumes = store.list_volumes()?;
    if volumes.is_empty() {
        println!("no volumes — create one by running with `-v NAME:/path`");
        return Ok(0);
    }
    println!("{:<32} {:>10}  CREATED FOR", "NAME", "SIZE");
    for v in volumes {
        println!(
            "{:<32} {:>10}  {}",
            v.name,
            human_bytes(v.size_bytes),
            v.created_for.as_deref().unwrap_or("-")
        );
    }
    Ok(0)
}

/// `myc volume inspect NAME`: metadata + host path.
pub fn volume_inspect(root: &Path, name: &str) -> Result<i32> {
    let store = open_store(root)?;
    let v = store.volume_info(name)?;
    println!("name:        {}", v.name);
    println!("size:        {}", human_bytes(v.size_bytes));
    println!("path:        {}", v.path.display());
    if let Some(win) = windows_path_for(&v.path) {
        println!("windows:     {win}");
    }
    if let Some(created_for) = &v.created_for {
        println!("created for: {created_for}");
    }
    if v.created_at > 0 {
        println!("created at:  {} (unix)", v.created_at);
    }
    Ok(0)
}

/// `myc volume rm NAME`: delete a volume and its data.
pub fn volume_rm(root: &Path, name: &str) -> Result<i32> {
    let store = open_store(root)?;
    let v = store.volume_info(name)?;
    store.remove_volume(name)?;
    println!("removed volume {} ({})", name, human_bytes(v.size_bytes));
    Ok(0)
}

/// The `\\wsl.localhost\<distro>\...` form of a WSL path, when running
/// inside WSL (lets Windows users open the folder in Explorer).
fn windows_path_for(path: &Path) -> Option<String> {
    let distro = std::env::var("WSL_DISTRO_NAME").ok()?;
    Some(format!(
        "\\\\wsl.localhost\\{distro}{}",
        path.display().to_string().replace('/', "\\")
    ))
}

/// `myc ui`: local web dashboard.
pub fn ui(root: &Path, port: u16) -> Result<i32> {
    myc_web::serve(root.to_path_buf(), port)?;
    Ok(0)
}

/// `myc` with no arguments: a guided welcome instead of a usage error.
pub fn welcome(root: &Path) -> Result<i32> {
    let paint = Paint::auto();
    println!(
        "{}",
        paint.bold("mycel — the content-addressed container runtime")
    );
    println!();
    println!("Environments are stored as file graphs: every file lives once in a local");
    println!("store, and \"images\" are tiny manifests pointing at those files. No daemon,");
    println!("no root, no duplicate layers.");
    println!();
    println!("{}", paint.bold("The five commands you actually need:"));
    let commands: [(&str, &str); 5] = [
        (
            "myc shell alpine:3.20",
            "interactive shell in any Docker Hub image",
        ),
        (
            "myc run alpine:3.20 -- echo hi",
            "run a command in an environment",
        ),
        ("myc ls", "list what is in your store"),
        (
            "myc diff alpine:3.20 alpine:3.21",
            "exact file diff between two environments",
        ),
        ("myc ui", "open the web dashboard (http://localhost:7777)"),
    ];
    for (cmd, what) in commands {
        // Pad before painting so ANSI escapes don't skew the column.
        println!(
            "  {} {}",
            paint.cyan(&format!("{cmd:<36}")),
            paint.dim(what)
        );
    }
    println!();
    println!(
        "{}",
        paint.dim("Something not working? `myc doctor` diagnoses it. Full list: `myc --help`.")
    );
    println!();

    // One-line store status.
    match Store::open(root) {
        Ok(store) => {
            let manifests = store.list_manifests().unwrap_or_default();
            if manifests.is_empty() {
                println!(
                    "store: {} — empty. Try: {}",
                    root.display(),
                    paint.bold("myc shell alpine:3.20")
                );
            } else {
                let stats = store.blob_stats().ok();
                println!(
                    "store: {} — {} environment(s), {} on disk",
                    root.display(),
                    manifests.len(),
                    stats
                        .map(|s| human_bytes(s.bytes))
                        .unwrap_or_else(|| "?".into()),
                );
            }
        }
        Err(e) => println!("store: {} — cannot open ({e})", root.display()),
    }
    Ok(0)
}

/// Rewrites a project-relative bind into an absolute HOST:CONTAINER[:ro] spec.
fn resolve_bind_to_spec(spec: &str, base: &Path) -> Result<String> {
    let (host, container, ro) =
        myc_compose::parse_bind(spec, base).map_err(|e| anyhow::anyhow!("{e}"))?;
    let host = host
        .canonicalize()
        .with_context(|| format!("bind source does not exist: {}", host.display()))?;
    Ok(if ro {
        format!("{}:{container}:ro", host.display())
    } else {
        format!("{}:{container}", host.display())
    })
}

#[cfg(test)]
mod tests {
    use super::{build_network, parse_publish};
    use myc_run::{Network, PortMap};

    #[test]
    fn publish_specs_are_parsed() {
        assert_eq!(
            parse_publish("6380:6379").unwrap(),
            PortMap {
                host: 6380,
                container: 6379
            }
        );
        assert_eq!(
            parse_publish("8080").unwrap(),
            PortMap {
                host: 8080,
                container: 8080
            }
        );
        assert!(parse_publish("").is_err());
        assert!(parse_publish("abc:80").is_err());
        assert!(parse_publish("80:").is_err());
        assert!(parse_publish("0:80").is_err());
        assert!(parse_publish("99999:80").is_err());
    }

    #[test]
    fn network_mode_is_built_from_flags() {
        assert_eq!(build_network("host", &[]).unwrap(), Network::Host);
        assert_eq!(build_network("none", &[]).unwrap(), Network::Loopback);
        assert_eq!(
            build_network("isolated", &[]).unwrap(),
            Network::Isolated {
                publish: Vec::new()
            }
        );
        // -p implies isolation even without --net isolated.
        assert_eq!(
            build_network("host", &["6380:6379".to_string()]).unwrap(),
            Network::Isolated {
                publish: vec![PortMap {
                    host: 6380,
                    container: 6379
                }]
            }
        );
        assert!(build_network("none", &["80".to_string()]).is_err());
        assert!(build_network("bridge", &[]).is_err());
    }
}
