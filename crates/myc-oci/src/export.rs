//! OCI image export: the reverse of ingestion, and the proof of no lock-in.
//!
//! Rebuilds a standard OCI image from a Mycel manifest + the content store:
//! one deterministic gzip layer containing the whole rootfs, an OCI config
//! derived from the manifest's `RuntimeConfig`, and a tar that carries BOTH
//! an OCI layout (`oci-layout`, `index.json`, `blobs/sha256/...`) and the
//! legacy Docker `manifest.json` (docker-save format, with `RepoTags`), so
//! `docker load`, podman, skopeo and crane all accept the same file.
//!
//! Determinism: manifest entries are already sorted by path, every tar
//! header uses mtime 0, and gzip is written with a zero timestamp — two
//! exports of the same manifest produce byte-identical archives.

use crate::{OciError, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use myc_manifest::{Entry, EntryKind, Manifest};
use myc_store::Store;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};

const MT_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const MT_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MT_OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const MT_OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// Outcome of [`export_oci_image`].
#[derive(Debug)]
pub struct OciExportReport {
    /// `name:tag` the image is loadable under (`docker load` prints it).
    pub repo_tag: String,
    /// sha256 digest of the (gzip) layer blob, `sha256:...`.
    pub layer_digest: String,
    /// sha256 of the uncompressed layer tar (the config's `diff_id`).
    pub layer_diff_id: String,
    /// sha256 digest of the OCI image config blob.
    pub config_digest: String,
    /// sha256 digest of the OCI image manifest blob.
    pub manifest_digest: String,
    /// Compressed layer size in bytes.
    pub layer_size: u64,
    /// Regular files written into the layer.
    pub files: usize,
}

/// Counts and sha256-hashes everything written through it.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    count: u64,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        HashingWriter {
            inner,
            hasher: Sha256::new(),
            count: 0,
        }
    }

    /// `(sha256 hex, bytes written, inner writer)`.
    fn finish(self) -> (String, u64, W) {
        (hex::encode(self.hasher.finalize()), self.count, self.inner)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The `name:tag` docker should list the image under: the manifest name,
/// with `:latest` appended when it carries no tag.
fn repo_tag(name: &str) -> String {
    let last = name.rsplit('/').next().unwrap_or(name);
    if last.contains(':') {
        name.to_string()
    } else {
        format!("{name}:latest")
    }
}

fn empty_obj() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

// ---- JSON documents (field order fixed by struct order: deterministic) ----

#[derive(Serialize)]
struct ConfigJson<'a> {
    architecture: &'a str,
    config: RuntimeJson<'a>,
    created: &'a str,
    history: [HistoryJson<'a>; 1],
    os: &'a str,
    rootfs: RootfsJson,
}

#[derive(Serialize)]
struct RuntimeJson<'a> {
    #[serde(rename = "Env", skip_serializing_if = "<[String]>::is_empty")]
    env: &'a [String],
    #[serde(rename = "Entrypoint", skip_serializing_if = "<[String]>::is_empty")]
    entrypoint: &'a [String],
    #[serde(rename = "Cmd", skip_serializing_if = "<[String]>::is_empty")]
    cmd: &'a [String],
    #[serde(rename = "WorkingDir", skip_serializing_if = "str::is_empty")]
    working_dir: &'a str,
    #[serde(rename = "User", skip_serializing_if = "str::is_empty")]
    user: &'a str,
    #[serde(rename = "ExposedPorts", skip_serializing_if = "BTreeMap::is_empty")]
    exposed_ports: BTreeMap<String, serde_json::Value>,
    #[serde(rename = "Volumes", skip_serializing_if = "BTreeMap::is_empty")]
    volumes: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct HistoryJson<'a> {
    created: &'a str,
    created_by: &'a str,
    comment: &'a str,
}

#[derive(Serialize)]
struct RootfsJson {
    #[serde(rename = "type")]
    kind: &'static str,
    diff_ids: [String; 1],
}

#[derive(Serialize)]
struct DescriptorJson {
    #[serde(rename = "mediaType")]
    media_type: &'static str,
    digest: String,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotations: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<PlatformJson>,
}

#[derive(Serialize)]
struct PlatformJson {
    architecture: String,
    os: String,
}

#[derive(Serialize)]
struct ManifestJson {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType")]
    media_type: &'static str,
    config: DescriptorJson,
    layers: Vec<DescriptorJson>,
}

#[derive(Serialize)]
struct IndexJson {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType")]
    media_type: &'static str,
    manifests: Vec<DescriptorJson>,
}

/// One record of the legacy `manifest.json` (docker-save format).
#[derive(Serialize)]
struct DockerManifestJson {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Vec<String>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

// ---- layer construction ----

/// Append one manifest entry to the layer tar. All headers use mtime 0 so
/// the layer bytes depend only on the manifest + blob contents.
fn append_entry<W: Write>(
    store: &Store,
    builder: &mut tar::Builder<W>,
    entry: &Entry,
) -> Result<()> {
    let rel = entry.path.trim_start_matches('/');
    let mut header = tar::Header::new_gnu();
    header.set_mtime(0);
    header.set_uid(entry.uid as u64);
    header.set_gid(entry.gid as u64);
    header.set_mode(entry.mode);
    header.set_size(0);

    match entry.kind {
        EntryKind::Dir => {
            header.set_entry_type(tar::EntryType::Directory);
            builder
                .append_data(&mut header, format!("{rel}/"), std::io::empty())
                .map_err(OciError::Io)?;
        }
        EntryKind::File => {
            let hash = entry
                .blake3
                .as_deref()
                .ok_or_else(|| OciError::Layer(format!("file without hash: {}", entry.path)))?;
            let blob = store.blob(hash)?;
            let file = std::fs::File::open(&blob).map_err(OciError::Io)?;
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(entry.size);
            builder
                .append_data(&mut header, rel, file)
                .map_err(OciError::Io)?;
        }
        EntryKind::Symlink => {
            let target = entry.target.as_deref().ok_or_else(|| {
                OciError::Layer(format!("symlink without target: {}", entry.path))
            })?;
            header.set_entry_type(tar::EntryType::Symlink);
            builder
                .append_link(&mut header, rel, target)
                .map_err(OciError::Io)?;
        }
        EntryKind::Fifo => {
            header.set_entry_type(tar::EntryType::Fifo);
            builder
                .append_data(&mut header, rel, std::io::empty())
                .map_err(OciError::Io)?;
        }
        EntryKind::CharDevice | EntryKind::BlockDevice => {
            let (major, minor) = entry.device.unwrap_or((0, 0));
            header.set_entry_type(if entry.kind == EntryKind::CharDevice {
                tar::EntryType::Char
            } else {
                tar::EntryType::Block
            });
            header
                .set_device_major(major as u32)
                .map_err(OciError::Io)?;
            header
                .set_device_minor(minor as u32)
                .map_err(OciError::Io)?;
            builder
                .append_data(&mut header, rel, std::io::empty())
                .map_err(OciError::Io)?;
        }
    }
    Ok(())
}

/// Rebuild the whole rootfs as a single gzip tar layer written to a temp
/// file. Returns `(diff_id hex, compressed sha256 hex, compressed size,
/// reopened temp file at offset 0)`.
fn build_layer(store: &Store, manifest: &Manifest) -> Result<(String, String, u64, std::fs::File)> {
    let tmp_dir = store.root().join("tmp");
    let file = tempfile::tempfile_in(&tmp_dir).map_err(OciError::Io)?;

    // tar -> diff_id hasher -> gzip -> compressed hasher -> temp file.
    let compressed = HashingWriter::new(file);
    let gz = GzEncoder::new(compressed, Compression::default());
    let uncompressed = HashingWriter::new(gz);
    let mut builder = tar::Builder::new(uncompressed);

    for entry in &manifest.entries {
        append_entry(store, &mut builder, entry)?;
    }

    let uncompressed = builder.into_inner().map_err(OciError::Io)?;
    let (diff_id, _, gz) = uncompressed.finish();
    let compressed = gz.finish().map_err(OciError::Io)?;
    let (layer_sha, layer_size, mut file) = compressed.finish();
    file.flush().map_err(OciError::Io)?;
    file.seek(SeekFrom::Start(0)).map_err(OciError::Io)?;
    Ok((diff_id, layer_sha, layer_size, file))
}

// ---- outer archive helpers ----

fn append_bytes<W: Write>(
    builder: &mut tar::Builder<W>,
    path: &str,
    data: &[u8],
) -> std::io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mode(0o644);
    header.set_size(data.len() as u64);
    builder.append_data(&mut header, path, data)
}

fn append_dir<W: Write>(builder: &mut tar::Builder<W>, path: &str) -> std::io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mode(0o755);
    header.set_size(0);
    header.set_entry_type(tar::EntryType::Directory);
    builder.append_data(&mut header, path, std::io::empty())
}

fn append_stream<W: Write, R: Read>(
    builder: &mut tar::Builder<W>,
    path: &str,
    size: u64,
    reader: R,
) -> std::io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mode(0o644);
    header.set_size(size);
    builder.append_data(&mut header, path, reader)
}

/// Export `manifest` as a standard OCI image tar written to `out`.
///
/// The archive is simultaneously a valid OCI image layout (for skopeo,
/// podman, crane, containerd) and a valid `docker save` tarball (for
/// `docker load`), and the export is deterministic: same manifest, same
/// bytes.
pub fn export_oci_image<W: Write>(
    store: &Store,
    manifest: &Manifest,
    out: W,
) -> Result<OciExportReport> {
    let missing = store.missing_blobs(manifest);
    if !missing.is_empty() {
        return Err(OciError::MissingBlobs(missing.len()));
    }

    let (diff_id, layer_sha, layer_size, layer_file) = build_layer(store, manifest)?;
    let tag = repo_tag(&manifest.name);

    // OCI image config, rebuilt from the manifest's RuntimeConfig.
    let exposed_ports: BTreeMap<String, serde_json::Value> = manifest
        .config
        .exposed_ports
        .iter()
        .map(|p| (format!("{p}/tcp"), empty_obj()))
        .collect();
    let volumes: BTreeMap<String, serde_json::Value> = manifest
        .config
        .volumes
        .iter()
        .map(|v| (v.clone(), empty_obj()))
        .collect();
    let config_json = ConfigJson {
        architecture: &manifest.arch,
        config: RuntimeJson {
            env: &manifest.config.env,
            entrypoint: &manifest.config.entrypoint,
            cmd: &manifest.config.cmd,
            working_dir: &manifest.config.workdir,
            user: &manifest.config.user,
            exposed_ports,
            volumes,
        },
        created: &manifest.created,
        history: [HistoryJson {
            created: &manifest.created,
            created_by: "mycel export --oci (single-layer rootfs rebuild)",
            comment: "exported from a Mycel content-addressed manifest",
        }],
        os: &manifest.os,
        rootfs: RootfsJson {
            kind: "layers",
            diff_ids: [format!("sha256:{diff_id}")],
        },
    };
    let config_bytes = serde_json::to_vec(&config_json)
        .map_err(|e| OciError::Layer(format!("config serialization: {e}")))?;
    let config_sha = hex::encode(Sha256::digest(&config_bytes));

    // OCI image manifest.
    let manifest_json = ManifestJson {
        schema_version: 2,
        media_type: MT_OCI_MANIFEST,
        config: DescriptorJson {
            media_type: MT_OCI_CONFIG,
            digest: format!("sha256:{config_sha}"),
            size: config_bytes.len() as u64,
            annotations: None,
            platform: None,
        },
        layers: vec![DescriptorJson {
            media_type: MT_OCI_LAYER_GZIP,
            digest: format!("sha256:{layer_sha}"),
            size: layer_size,
            annotations: None,
            platform: None,
        }],
    };
    let manifest_bytes = serde_json::to_vec(&manifest_json)
        .map_err(|e| OciError::Layer(format!("manifest serialization: {e}")))?;
    let manifest_sha = hex::encode(Sha256::digest(&manifest_bytes));

    // OCI index. The ref.name annotation is what tags the image when an
    // OCI-aware `docker load` / `podman load` reads the layout directly.
    let index_json = IndexJson {
        schema_version: 2,
        media_type: MT_OCI_INDEX,
        manifests: vec![DescriptorJson {
            media_type: MT_OCI_MANIFEST,
            digest: format!("sha256:{manifest_sha}"),
            size: manifest_bytes.len() as u64,
            annotations: Some(BTreeMap::from([(
                "org.opencontainers.image.ref.name".to_string(),
                tag.clone(),
            )])),
            platform: Some(PlatformJson {
                architecture: manifest.arch.clone(),
                os: manifest.os.clone(),
            }),
        }],
    };
    let index_bytes = serde_json::to_vec(&index_json)
        .map_err(|e| OciError::Layer(format!("index serialization: {e}")))?;

    // Legacy docker-save manifest, for `docker load` on older engines.
    let docker_manifest = vec![DockerManifestJson {
        config: format!("blobs/sha256/{config_sha}"),
        repo_tags: vec![tag.clone()],
        layers: vec![format!("blobs/sha256/{layer_sha}")],
    }];
    let docker_bytes = serde_json::to_vec(&docker_manifest)
        .map_err(|e| OciError::Layer(format!("docker manifest serialization: {e}")))?;

    // Assemble the output tar in a fixed order (deterministic archive).
    let mut builder = tar::Builder::new(out);
    append_bytes(
        &mut builder,
        "oci-layout",
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .map_err(OciError::Io)?;
    append_dir(&mut builder, "blobs/").map_err(OciError::Io)?;
    append_dir(&mut builder, "blobs/sha256/").map_err(OciError::Io)?;

    // Blobs sorted by digest (the layer is streamed from its temp file).
    let mut blobs: Vec<(&str, BlobSource)> = vec![
        (&config_sha, BlobSource::Bytes(&config_bytes)),
        (&manifest_sha, BlobSource::Bytes(&manifest_bytes)),
        (&layer_sha, BlobSource::File(layer_file, layer_size)),
    ];
    blobs.sort_by(|a, b| a.0.cmp(b.0));
    for (sha, source) in blobs {
        let path = format!("blobs/sha256/{sha}");
        match source {
            BlobSource::Bytes(data) => append_bytes(&mut builder, &path, data),
            BlobSource::File(file, size) => append_stream(&mut builder, &path, size, file),
        }
        .map_err(OciError::Io)?;
    }

    append_bytes(&mut builder, "index.json", &index_bytes).map_err(OciError::Io)?;
    append_bytes(&mut builder, "manifest.json", &docker_bytes).map_err(OciError::Io)?;
    builder.finish().map_err(OciError::Io)?;

    Ok(OciExportReport {
        repo_tag: tag,
        layer_digest: format!("sha256:{layer_sha}"),
        layer_diff_id: format!("sha256:{diff_id}"),
        config_digest: format!("sha256:{config_sha}"),
        manifest_digest: format!("sha256:{manifest_sha}"),
        layer_size,
        files: manifest.file_count(),
    })
}

enum BlobSource<'a> {
    Bytes(&'a [u8]),
    File(std::fs::File, u64),
}

#[cfg(test)]
mod tests {
    use super::*;
    use myc_manifest::{RuntimeConfig, SCHEMA_VERSION};

    /// A small but representative environment: dirs, files (one duplicated
    /// content, i.e. a manifest-level hardlink), a symlink, full config.
    fn sample(store: &Store) -> Manifest {
        let (hash_app, size_app) = store.put_blob(&mut &b"#!app-binary"[..]).unwrap();
        let (hash_conf, size_conf) = store.put_blob(&mut &b"key=value\n"[..]).unwrap();
        let file = |path: &str, hash: &str, size: u64, mode: u32| Entry {
            path: path.into(),
            kind: EntryKind::File,
            mode,
            uid: 0,
            gid: 0,
            size,
            blake3: Some(hash.into()),
            target: None,
            device: None,
        };
        let dir = |path: &str| Entry {
            path: path.into(),
            kind: EntryKind::Dir,
            mode: 0o755,
            uid: 0,
            gid: 0,
            size: 0,
            blake3: None,
            target: None,
            device: None,
        };
        Manifest {
            schema: SCHEMA_VERSION,
            name: "test/oci-export:1".into(),
            origin: None,
            os: "linux".into(),
            arch: "amd64".into(),
            created: "2026-01-01T00:00:00Z".into(),
            config: RuntimeConfig {
                env: vec!["PATH=/bin".into(), "MODE=prod".into()],
                entrypoint: vec!["/bin/app".into()],
                cmd: vec!["--serve".into()],
                workdir: "/srv".into(),
                user: "1000".into(),
                exposed_ports: vec![8080],
                volumes: vec!["/data".into()],
            },
            entries: vec![
                dir("/bin"),
                file("/bin/app", &hash_app, size_app, 0o755),
                Entry {
                    path: "/bin/app-link".into(),
                    kind: EntryKind::Symlink,
                    mode: 0o777,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    blake3: None,
                    target: Some("app".into()),
                    device: None,
                },
                dir("/etc"),
                file("/etc/app.conf", &hash_conf, size_conf, 0o644),
                // Same content as /bin/app: a hardlink in the source image.
                file("/opt-copy", &hash_app, size_app, 0o755),
            ],
        }
    }

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        (dir, store)
    }

    fn export_to_vec(store: &Store, manifest: &Manifest) -> (Vec<u8>, OciExportReport) {
        let mut buf = Vec::new();
        let report = export_oci_image(store, manifest, &mut buf).unwrap();
        (buf, report)
    }

    /// Every archive member, fully read.
    fn read_members(tar_bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut ar = tar::Archive::new(tar_bytes);
        let mut out = BTreeMap::new();
        for entry in ar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            out.insert(path, data);
        }
        out
    }

    #[test]
    fn export_is_deterministic() {
        let (_d, store) = temp_store();
        let manifest = sample(&store);
        let (a, ra) = export_to_vec(&store, &manifest);
        let (b, rb) = export_to_vec(&store, &manifest);
        assert_eq!(a, b, "two exports of the same manifest must be identical");
        assert_eq!(ra.layer_digest, rb.layer_digest);
        assert_eq!(ra.manifest_digest, rb.manifest_digest);
    }

    #[test]
    fn archive_has_both_formats_and_coherent_digests() {
        let (_d, store) = temp_store();
        let manifest = sample(&store);
        let (bytes, report) = export_to_vec(&store, &manifest);
        let members = read_members(&bytes);

        // Both formats present in one tar.
        assert!(members.contains_key("oci-layout"));
        assert!(members.contains_key("index.json"));
        assert!(members.contains_key("manifest.json"));
        assert_eq!(
            members["oci-layout"],
            br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec()
        );

        // index -> manifest blob, digest verified.
        let index: serde_json::Value = serde_json::from_slice(&members["index.json"]).unwrap();
        let mdigest = index["manifests"][0]["digest"].as_str().unwrap();
        assert_eq!(mdigest, report.manifest_digest);
        assert_eq!(
            index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
            "test/oci-export:1"
        );
        let mhex = mdigest.strip_prefix("sha256:").unwrap();
        let mbytes = &members[&format!("blobs/sha256/{mhex}")];
        assert_eq!(hex::encode(Sha256::digest(mbytes)), mhex);

        // manifest -> config + layer blobs, digests verified.
        let oci_manifest: serde_json::Value = serde_json::from_slice(mbytes).unwrap();
        let chex = oci_manifest["config"]["digest"]
            .as_str()
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap()
            .to_string();
        let lhex = oci_manifest["layers"][0]["digest"]
            .as_str()
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap()
            .to_string();
        let cbytes = &members[&format!("blobs/sha256/{chex}")];
        let lbytes = &members[&format!("blobs/sha256/{lhex}")];
        assert_eq!(hex::encode(Sha256::digest(cbytes)), chex);
        assert_eq!(hex::encode(Sha256::digest(lbytes)), lhex);
        assert_eq!(lbytes.len() as u64, report.layer_size);

        // diff_id = sha256 of the uncompressed layer tar.
        let mut inner_tar = Vec::new();
        flate2::read::GzDecoder::new(lbytes.as_slice())
            .read_to_end(&mut inner_tar)
            .unwrap();
        let diff_id = format!("sha256:{}", hex::encode(Sha256::digest(&inner_tar)));
        let config: serde_json::Value = serde_json::from_slice(cbytes).unwrap();
        assert_eq!(config["rootfs"]["diff_ids"][0], diff_id.as_str());
        assert_eq!(diff_id, report.layer_diff_id);

        // Legacy docker manifest points at the same blobs.
        let docker: serde_json::Value = serde_json::from_slice(&members["manifest.json"]).unwrap();
        assert_eq!(docker[0]["Config"], format!("blobs/sha256/{chex}"));
        assert_eq!(docker[0]["Layers"][0], format!("blobs/sha256/{lhex}"));
        assert_eq!(docker[0]["RepoTags"][0], "test/oci-export:1");
    }

    #[test]
    fn config_carries_the_runtime_config() {
        let (_d, store) = temp_store();
        let manifest = sample(&store);
        let (bytes, report) = export_to_vec(&store, &manifest);
        let members = read_members(&bytes);
        let chex = report.config_digest.strip_prefix("sha256:").unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&members[&format!("blobs/sha256/{chex}")]).unwrap();

        assert_eq!(config["architecture"], "amd64");
        assert_eq!(config["os"], "linux");
        assert_eq!(config["created"], "2026-01-01T00:00:00Z");
        assert_eq!(config["config"]["Env"][0], "PATH=/bin");
        assert_eq!(config["config"]["Entrypoint"][0], "/bin/app");
        assert_eq!(config["config"]["Cmd"][0], "--serve");
        assert_eq!(config["config"]["WorkingDir"], "/srv");
        assert_eq!(config["config"]["User"], "1000");
        assert!(config["config"]["ExposedPorts"]["8080/tcp"].is_object());
        assert!(config["config"]["Volumes"]["/data"].is_object());
        assert_eq!(config["history"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn layer_reconstructs_the_rootfs() {
        let (_d, store) = temp_store();
        let manifest = sample(&store);
        let (bytes, report) = export_to_vec(&store, &manifest);
        let members = read_members(&bytes);
        let lhex = report.layer_digest.strip_prefix("sha256:").unwrap();
        let mut inner_tar = Vec::new();
        flate2::read::GzDecoder::new(members[&format!("blobs/sha256/{lhex}")].as_slice())
            .read_to_end(&mut inner_tar)
            .unwrap();

        let mut ar = tar::Archive::new(inner_tar.as_slice());
        let mut seen = BTreeMap::new();
        for entry in ar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            let kind = entry.header().entry_type();
            let mode = entry.header().mode().unwrap();
            let link = entry
                .link_name()
                .unwrap()
                .map(|p| p.to_string_lossy().to_string());
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            seen.insert(path, (kind, mode, link, data));
        }

        assert_eq!(seen["bin/"].0, tar::EntryType::Directory);
        let app = &seen["bin/app"];
        assert_eq!(app.0, tar::EntryType::Regular);
        assert_eq!(app.1, 0o755);
        assert_eq!(app.3, b"#!app-binary");
        let link = &seen["bin/app-link"];
        assert_eq!(link.0, tar::EntryType::Symlink);
        assert_eq!(link.2.as_deref(), Some("app"));
        assert_eq!(seen["etc/app.conf"].3, b"key=value\n");
        // Duplicate content is written twice (hardlinks become plain files).
        assert_eq!(seen["opt-copy"].3, b"#!app-binary");
        assert_eq!(report.files, 3);
    }

    #[test]
    fn missing_blob_is_refused() {
        let (_d, store) = temp_store();
        let mut manifest = sample(&store);
        manifest.entries.push(Entry {
            path: "/zz-missing".into(),
            kind: EntryKind::File,
            mode: 0o644,
            uid: 0,
            gid: 0,
            size: 1,
            blake3: Some("0".repeat(64)),
            target: None,
            device: None,
        });
        let err = export_oci_image(&store, &manifest, Vec::new()).unwrap_err();
        assert!(matches!(err, OciError::MissingBlobs(1)));
    }

    #[test]
    fn untagged_names_get_latest() {
        assert_eq!(repo_tag("myapp"), "myapp:latest");
        assert_eq!(repo_tag("org/app"), "org/app:latest");
        assert_eq!(repo_tag("app:1.2"), "app:1.2");
        assert_eq!(
            repo_tag("registry:5000/org/app"),
            "registry:5000/org/app:latest"
        );
        assert_eq!(
            repo_tag("registry:5000/org/app:v2"),
            "registry:5000/org/app:v2"
        );
    }
}
