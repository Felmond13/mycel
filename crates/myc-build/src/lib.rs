//! Native image builds: no Dockerfile, no daemon, no layers.
//!
//! A build is described by a small `mycel-build.toml` (see `spec`): an
//! optional base image, files to add, ordered exec steps, and runtime
//! config overrides. The result is an ordinary Mycel manifest — files added
//! on top of an unchanged base cost exactly their own bytes, because
//! everything else is already in the content-addressed store.
//!
//! The crate is registry-agnostic: callers resolve the base image
//! themselves (the CLI reuses its ingest-on-miss logic) and hand over the
//! base `Manifest`.

pub mod add;
pub mod exec;
pub mod scan;
pub mod spec;
pub mod timestamp;
pub mod tree;

pub use exec::StepSummary;
pub use spec::BuildSpec;

use myc_manifest::{Manifest, SCHEMA_VERSION};
use myc_store::Store;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("invalid build spec: {0}")]
    Spec(String),
    #[error("add source does not exist: {0}")]
    MissingSource(PathBuf),
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] myc_store::StoreError),
    #[error(transparent)]
    Manifest(#[from] myc_manifest::ManifestError),
    #[error(transparent)]
    Run(#[from] myc_run::RunError),
    #[error("build step {index} failed with exit code {code}")]
    StepFailed { index: usize, code: i32 },
    #[error("build has no name: set build.name in the toml or pass -t NAME:TAG")]
    NoName,
    #[error("bad timestamp: {0}")]
    Timestamp(String),
}

pub type Result<T> = std::result::Result<T, BuildError>;

/// Everything a build needs beyond the store. The base manifest, if any,
/// is resolved by the caller so this crate stays decoupled from registries.
pub struct BuildRequest<'a> {
    pub spec: &'a BuildSpec,
    /// Directory `add.source` paths are relative to (the toml's parent).
    pub context_dir: &'a Path,
    /// Resolved base manifest (`build.base`), if the spec declares one.
    pub base: Option<&'a Manifest>,
    /// `-t NAME:TAG` override; wins over `build.name`.
    pub name_override: Option<&'a str>,
    /// Explicit timestamp (`--timestamp`); else `SOURCE_DATE_EPOCH`, else now.
    pub timestamp: Option<&'a str>,
    /// Recorded as the manifest origin, e.g. `build://mycel-build.toml`.
    pub origin: Option<String>,
    /// Scratch dir for step rootfs materialization (only used by run steps).
    pub work_dir: &'a Path,
}

/// Progress callbacks so the CLI can narrate without the library knowing
/// anything about terminals.
pub trait BuildProgress {
    fn add_done(&mut self, _source: &str, _dest: &str, _stats: &add::AddStats) {}
    fn step_started(&mut self, _index: usize, _total: usize, _command: &[String]) {}
    fn step_done(&mut self, _index: usize, _summary: &StepSummary) {}
}

pub struct NoProgress;
impl BuildProgress for NoProgress {}

#[derive(Debug)]
pub struct BuildReport {
    pub manifest_id: String,
    pub name: String,
    pub file_count: usize,
    pub logical_size: u64,
    /// Bytes actually written to the store; the rest was already there.
    pub bytes_added: u64,
    pub blobs_added: u64,
    pub steps: Vec<StepSummary>,
}

/// Execute a build: base entries + adds + run steps -> stored manifest.
/// The manifest is validated, stored, and a ref is set to the result name.
pub fn build(
    store: &Store,
    req: &BuildRequest,
    progress: &mut dyn BuildProgress,
) -> Result<BuildReport> {
    let spec = req.spec;
    let name = req
        .name_override
        .or(spec.build.name.as_deref())
        .ok_or(BuildError::NoName)?
        .to_string();
    let created = timestamp::resolve(req.timestamp).map_err(BuildError::Timestamp)?;

    // Start from the base's file tree (or empty for scratch builds).
    let mut file_tree = match req.base {
        Some(base) => tree::from_entries(&base.entries),
        None => tree::FileTree::new(),
    };

    // Final config up front: run steps execute with the merged env/workdir,
    // like Dockerfile RUN honoring ENV/WORKDIR.
    let config = tree::merge_config(req.base.map(|b| &b.config), spec.config.as_ref());

    let mut bytes_added = 0u64;
    let mut blobs_added = 0u64;

    // 1. adds (always before run steps).
    for add_spec in &spec.build.add {
        let stats = add::apply_add(store, &mut file_tree, add_spec, req.context_dir)?;
        bytes_added += stats.bytes_added;
        blobs_added += stats.blobs_added;
        progress.add_done(&add_spec.source, &add_spec.dest, &stats);
    }

    // 2. run steps, in order.
    let mut steps = Vec::new();
    let total = spec.build.run.len();
    for (i, step) in spec.build.run.iter().enumerate() {
        progress.step_started(i, total, &step.command);
        let summary = exec::run_step(store, &mut file_tree, &config, step, i, req.work_dir)?;
        bytes_added += summary.bytes_added;
        blobs_added += summary.blobs_added;
        progress.step_done(i, &summary);
        steps.push(summary);
    }

    let (os, arch) = match req.base {
        Some(base) => (base.os.clone(), base.arch.clone()),
        None => ("linux".to_string(), host_arch()),
    };

    let manifest = Manifest {
        schema: SCHEMA_VERSION,
        name: name.clone(),
        origin: req.origin.clone(),
        os,
        arch,
        created,
        config,
        entries: file_tree.into_values().collect(),
    };
    manifest.validate()?;

    let file_count = manifest.file_count();
    let logical_size = manifest.logical_size();
    let manifest_id = store.put_manifest(&manifest)?;
    store.set_ref(&name, &manifest_id)?;

    Ok(BuildReport {
        manifest_id,
        name,
        file_count,
        logical_size,
        bytes_added,
        blobs_added,
        steps,
    })
}

/// Host architecture in OCI naming (scratch builds only).
fn host_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
    .to_string()
}
