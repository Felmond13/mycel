//! The Mycel manifest: a small, canonical, content-addressed description of a
//! complete runnable filesystem plus its runtime configuration.
//!
//! A manifest replaces the OCI image: instead of opaque layered tarballs it is
//! an explicit list of every file in the environment, each identified by the
//! BLAKE3 hash of its content. Two manifests that share files share storage.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("invalid manifest: {0}")]
    Invalid(String),
}

/// Kind of filesystem node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Fifo,
    CharDevice,
    BlockDevice,
}

/// One node of the filesystem graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Absolute, normalized path inside the rootfs (always starts with `/`,
    /// never contains `.` or `..` components).
    pub path: String,
    pub kind: EntryKind,
    /// Unix permission bits (the low 12 bits: rwxrwxrwx + setuid/setgid/sticky).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Content size in bytes (0 for non-files).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    /// BLAKE3 hash of the file content (files only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
    /// Symlink target (symlinks only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Device major/minor (devices only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<(u64, u64)>,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// Process configuration, equivalent to the runtime half of an OCI image config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entrypoint: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workdir: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// TCP ports the image declares it listens on (OCI `ExposedPorts`).
    /// Skipped when empty, so manifests ingested before this field existed
    /// keep their canonical encoding — and therefore their id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exposed_ports: Vec<u16>,
    /// Container paths the image declares as data directories (OCI
    /// `Volumes`, e.g. `/var/lib/postgresql/data`), sorted. Skipped when
    /// empty for the same id-stability reason as `exposed_ports`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
}

/// A complete, self-describing runnable environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    /// Human-readable name (e.g. `docker.io/library/alpine:3.20`).
    pub name: String,
    /// Where this manifest was ingested from, if anywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub os: String,
    pub arch: String,
    /// RFC 3339 creation timestamp.
    pub created: String,
    pub config: RuntimeConfig,
    /// Sorted by path; parents always precede children.
    pub entries: Vec<Entry>,
}

impl Manifest {
    /// Canonical JSON encoding. Field order is fixed by the struct definition
    /// and entries are sorted, so equal manifests produce equal bytes.
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, ManifestError> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn to_pretty_json(&self) -> Result<String, ManifestError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, ManifestError> {
        let m: Manifest = serde_json::from_slice(bytes)?;
        m.validate()?;
        Ok(m)
    }

    /// The manifest id: BLAKE3 of the canonical encoding, hex, prefixed.
    pub fn id(&self) -> Result<String, ManifestError> {
        let bytes = self.to_canonical_json()?;
        Ok(format!("myc1-{}", blake3::hash(&bytes).to_hex()))
    }

    /// Total logical size (sum of all file sizes, before deduplication).
    pub fn logical_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    pub fn file_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .count()
    }

    /// The command to execute: entrypoint + cmd, with optional override.
    pub fn command(&self, override_cmd: &[String]) -> Vec<String> {
        let mut out = self.config.entrypoint.clone();
        if !override_cmd.is_empty() {
            out.extend(override_cmd.iter().cloned());
        } else {
            out.extend(self.config.cmd.iter().cloned());
        }
        out
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema != SCHEMA_VERSION {
            return Err(ManifestError::Invalid(format!(
                "unsupported schema version {}",
                self.schema
            )));
        }
        for e in &self.entries {
            if !e.path.starts_with('/') || e.path.contains("/../") || e.path.ends_with("/..") {
                return Err(ManifestError::Invalid(format!(
                    "entry path is not normalized: {}",
                    e.path
                )));
            }
            if e.kind == EntryKind::File && e.blake3.is_none() {
                return Err(ManifestError::Invalid(format!(
                    "file entry without content hash: {}",
                    e.path
                )));
            }
            if e.kind == EntryKind::Symlink && e.target.is_none() {
                return Err(ManifestError::Invalid(format!(
                    "symlink entry without target: {}",
                    e.path
                )));
            }
        }
        Ok(())
    }

    /// Set of unique content hashes referenced by this manifest.
    pub fn referenced_blobs(&self) -> BTreeMap<&str, u64> {
        let mut out = BTreeMap::new();
        for e in &self.entries {
            if let Some(h) = e.blake3.as_deref() {
                out.insert(h, e.size);
            }
        }
        out
    }
}

/// Normalize a path coming from a tar archive into `/a/b/c` form.
/// Returns `None` for paths that escape the root or are empty.
pub fn normalize_tar_path(raw: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for comp in raw.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("/{}", parts.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            schema: SCHEMA_VERSION,
            name: "test/app:1".into(),
            origin: None,
            os: "linux".into(),
            arch: "amd64".into(),
            created: "2026-01-01T00:00:00Z".into(),
            config: RuntimeConfig {
                env: vec!["PATH=/usr/bin".into()],
                cmd: vec!["/bin/sh".into()],
                ..Default::default()
            },
            entries: vec![
                Entry {
                    path: "/bin".into(),
                    kind: EntryKind::Dir,
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    blake3: None,
                    target: None,
                    device: None,
                },
                Entry {
                    path: "/bin/sh".into(),
                    kind: EntryKind::File,
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    size: 42,
                    blake3: Some("deadbeef".into()),
                    target: None,
                    device: None,
                },
            ],
        }
    }

    #[test]
    fn id_is_deterministic() {
        let a = sample();
        let b = sample();
        assert_eq!(a.id().unwrap(), b.id().unwrap());
        assert!(a.id().unwrap().starts_with("myc1-"));
    }

    #[test]
    fn roundtrip() {
        let m = sample();
        let bytes = m.to_canonical_json().unwrap();
        let back = Manifest::from_json(&bytes).unwrap();
        assert_eq!(m.id().unwrap(), back.id().unwrap());
    }

    #[test]
    fn tar_path_normalization() {
        assert_eq!(normalize_tar_path("./bin/sh").unwrap(), "/bin/sh");
        assert_eq!(normalize_tar_path("bin//sh").unwrap(), "/bin/sh");
        assert_eq!(normalize_tar_path("a/b/../c").unwrap(), "/a/c");
        assert!(normalize_tar_path("..").is_none());
        assert!(normalize_tar_path("").is_none());
    }

    #[test]
    fn empty_volumes_do_not_change_canonical_encoding() {
        // Manifests ingested before the `volumes` field existed must keep
        // their exact bytes — and therefore their content-addressed id.
        let m = sample();
        let json = String::from_utf8(m.to_canonical_json().unwrap()).unwrap();
        assert!(!json.contains("\"volumes\""));

        let mut with = sample();
        with.config.volumes = vec!["/data".into()];
        let json = String::from_utf8(with.to_canonical_json().unwrap()).unwrap();
        assert!(json.contains("\"volumes\":[\"/data\"]"));
        let back = Manifest::from_json(json.as_bytes()).unwrap();
        assert_eq!(back.config.volumes, vec!["/data".to_string()]);
        assert_ne!(m.id().unwrap(), with.id().unwrap());
    }

    #[test]
    fn command_override() {
        let m = sample();
        assert_eq!(m.command(&[]), vec!["/bin/sh".to_string()]);
        assert_eq!(
            m.command(&["echo".to_string(), "hi".to_string()]),
            vec!["echo".to_string(), "hi".to_string()]
        );
    }
}
