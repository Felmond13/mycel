//! OCI/Docker registry client and image ingestion.
//!
//! `ingest` pulls a real image from any OCI registry (Docker Hub, GHCR, Quay,
//! private registries), applies its layers in order — honoring whiteouts —
//! and produces a Mycel manifest: an explicit file graph stored, file by
//! file, in the content-addressed store.

mod export;
mod layer;
mod reference;
mod registry;

pub use export::{export_oci_image, OciExportReport};
pub use reference::ImageReference;
pub use registry::{Platform, RegistryClient};

use myc_manifest::{Entry, Manifest, RuntimeConfig, SCHEMA_VERSION};
use myc_store::Store;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum OciError {
    #[error("invalid image reference '{0}'")]
    BadReference(String),
    #[error("registry error: {0}")]
    Registry(String),
    #[error("registry returned HTTP {status} for {url}")]
    HttpStatus { url: String, status: u16 },
    #[error("image '{image}' not found on {registry} (or requires authentication) — check the name{hint}")]
    ImageNotFound {
        image: String,
        registry: String,
        hint: String,
    },
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unsupported media type: {0}")]
    UnsupportedMediaType(String),
    #[error("no image found for platform {os}/{arch}")]
    NoPlatformMatch { os: String, arch: String },
    #[error("layer digest mismatch: expected {expected}")]
    DigestMismatch { expected: String },
    #[error("layer apply error: {0}")]
    Layer(String),
    #[error("{0} blob(s) missing from the local store — run `myc pull` first")]
    MissingBlobs(usize),
    #[error(transparent)]
    Store(#[from] myc_store::StoreError),
    #[error(transparent)]
    Manifest(#[from] myc_manifest::ManifestError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, OciError>;

/// Progress callbacks so the CLI can render status without the library
/// knowing anything about terminals.
pub trait IngestProgress {
    fn layer_started(&mut self, index: usize, total: usize, digest: &str);
    fn layer_done(&mut self, index: usize, files_added: usize, bytes_added: u64);
}

pub struct NoProgress;
impl IngestProgress for NoProgress {
    fn layer_started(&mut self, _: usize, _: usize, _: &str) {}
    fn layer_done(&mut self, _: usize, _: usize, _: u64) {}
}

pub struct IngestReport {
    pub manifest_id: String,
    pub name: String,
    pub file_count: usize,
    pub logical_size: u64,
    pub blobs_added: u64,
    pub bytes_added: u64,
}

/// Check that Docker Hub's registry endpoint is reachable.
///
/// A `401 Unauthorized` counts as reachable: it proves DNS, TCP and TLS all
/// work — the registry merely wants a token, which ingestion negotiates.
/// Must be called from a plain (non-async) thread.
pub fn registry_reachable() -> std::result::Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(|e| e.to_string())?;
    match client.get("https://registry-1.docker.io/v2/").send() {
        Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 401 => Ok(()),
        Ok(resp) => Err(format!("unexpected status {}", resp.status())),
        Err(e) => Err(e.to_string()),
    }
}

/// Normalize an OCI `Volumes` key into `/a/b` form (no trailing slash, no
/// `.`/`..` components). Returns `None` for anything that is not a clean
/// absolute path.
fn normalize_volume_path(raw: &str) -> Option<String> {
    if !raw.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for comp in raw.split('/') {
        match comp {
            "" | "." => {}
            ".." => return None,
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("/{}", parts.join("/")))
}

/// Pull `image` from its registry and ingest it into `store`.
///
/// The resulting manifest is stored, and a ref `<normalized name>` is set.
pub fn ingest(
    store: &Store,
    image: &str,
    platform: &Platform,
    progress: &mut dyn IngestProgress,
) -> Result<IngestReport> {
    let reference = ImageReference::parse(image)?;
    let mut client = RegistryClient::new(&reference)?;

    let resolved = client.resolve_image(platform)?;
    let config = client.fetch_config(&resolved.config_digest)?;

    // The file tree being assembled: path -> entry. BTreeMap keeps paths
    // sorted, which gives us canonical manifests for free.
    let mut tree: BTreeMap<String, Entry> = BTreeMap::new();
    let mut blobs_added = 0u64;
    let mut bytes_added = 0u64;

    let total = resolved.layers.len();
    for (i, layer_desc) in resolved.layers.iter().enumerate() {
        progress.layer_started(i, total, &layer_desc.digest);
        let reader = client.fetch_blob(&layer_desc.digest)?;
        let stats = layer::apply_layer(
            store,
            &mut tree,
            reader,
            &layer_desc.media_type,
            &layer_desc.digest,
        )?;
        blobs_added += stats.blobs_added;
        bytes_added += stats.bytes_added;
        progress.layer_done(i, stats.files_added, stats.bytes_added);
    }

    // `ExposedPorts` keys look like "6379/tcp" (bare "6379" means tcp).
    // Only TCP matters here: the dashboard uses these for its port-conflict
    // preflight, which checks TCP listeners on the shared host network.
    let mut exposed_ports: Vec<u16> = config
        .config
        .exposed_ports
        .as_ref()
        .map(|ports| {
            ports
                .keys()
                .filter_map(|key| {
                    let (port, proto) = key.split_once('/').unwrap_or((key.as_str(), "tcp"));
                    if proto == "tcp" {
                        port.parse::<u16>().ok()
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    exposed_ports.sort_unstable();
    exposed_ports.dedup();

    // `Volumes` keys are container paths the image expects to persist
    // (e.g. postgres declares /var/lib/postgresql/data). Normalized to
    // `/a/b` form; anything not absolute is dropped.
    let mut volumes: Vec<String> = config
        .config
        .volumes
        .as_ref()
        .map(|vols| {
            vols.keys()
                .filter_map(|key| normalize_volume_path(key))
                .collect()
        })
        .unwrap_or_default();
    volumes.sort();
    volumes.dedup();

    let runtime_config = RuntimeConfig {
        env: config.config.env.unwrap_or_default(),
        entrypoint: config.config.entrypoint.unwrap_or_default(),
        cmd: config.config.cmd.unwrap_or_default(),
        workdir: config.config.working_dir.unwrap_or_default(),
        user: config.config.user.unwrap_or_default(),
        exposed_ports,
        volumes,
    };

    let manifest = Manifest {
        schema: SCHEMA_VERSION,
        name: reference.canonical_name(),
        origin: Some(format!("oci://{}", reference.canonical_name())),
        os: config.os.clone(),
        arch: config.architecture.clone(),
        created: config
            .created
            .clone()
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string()),
        config: runtime_config,
        entries: tree.into_values().collect(),
    };
    manifest.validate()?;

    let file_count = manifest.file_count();
    let logical_size = manifest.logical_size();
    let manifest_id = store.put_manifest(&manifest)?;
    store.set_ref(&reference.canonical_name(), &manifest_id)?;
    // Also set the short name the user typed (e.g. `alpine:3.20`).
    if reference.short_name() != reference.canonical_name() {
        store.set_ref(&reference.short_name(), &manifest_id)?;
    }

    Ok(IngestReport {
        manifest_id,
        name: reference.canonical_name(),
        file_count,
        logical_size,
        blobs_added,
        bytes_added,
    })
}

#[cfg(test)]
mod tests {
    use super::normalize_volume_path;

    #[test]
    fn volume_paths_are_normalized() {
        assert_eq!(normalize_volume_path("/data").unwrap(), "/data");
        assert_eq!(normalize_volume_path("/data/").unwrap(), "/data");
        assert_eq!(
            normalize_volume_path("/var/lib//postgresql/./data").unwrap(),
            "/var/lib/postgresql/data"
        );
        assert!(normalize_volume_path("data").is_none());
        assert!(normalize_volume_path("/data/../etc").is_none());
        assert!(normalize_volume_path("/").is_none());
    }
}
