//! Minimal, correct OCI Distribution API client (pull side).
//!
//! Handles the token auth dance (WWW-Authenticate: Bearer), manifest lists /
//! OCI indexes, platform selection, and streaming blob downloads with
//! sha256 verification.

use crate::reference::ImageReference;
use crate::{OciError, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;

const MT_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const MT_DOCKER_MANIFEST_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const MT_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const MT_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

#[derive(Debug, Clone)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

impl Platform {
    /// Host platform, translated to OCI naming.
    pub fn host() -> Self {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        Platform {
            os: "linux".to_string(), // containers are linux environments
            arch: arch.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    // Registries may return `token`, `access_token`, or both (Docker Hub).
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

impl TokenResponse {
    fn into_token(self) -> Option<String> {
        self.token.or(self.access_token)
    }
}

#[derive(Debug, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub digest: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    platform: Option<PlatformDesc>,
}

#[derive(Debug, Deserialize)]
struct PlatformDesc {
    os: String,
    architecture: String,
}

#[derive(Debug, Deserialize)]
struct ImageManifest {
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct ImageIndex {
    manifests: Vec<Descriptor>,
}

/// OCI image config (the runtime-relevant subset).
#[derive(Debug, Deserialize)]
pub struct ImageConfig {
    pub architecture: String,
    pub os: String,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub config: InnerConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct InnerConfig {
    #[serde(rename = "Env")]
    pub env: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd")]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    pub working_dir: Option<String>,
    #[serde(rename = "User")]
    pub user: Option<String>,
    /// Keys are `"6379/tcp"`-style specs; values are always empty objects.
    #[serde(rename = "ExposedPorts")]
    pub exposed_ports: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    /// Keys are container paths (`"/data"`); values are always empty objects.
    #[serde(rename = "Volumes")]
    pub volumes: Option<std::collections::BTreeMap<String, serde_json::Value>>,
}

pub struct ResolvedImage {
    pub config_digest: String,
    pub layers: Vec<Descriptor>,
}

pub struct RegistryClient {
    http: reqwest::blocking::Client,
    base: String,
    /// Registry host for error messages (e.g. `docker.io`).
    registry: String,
    repository: String,
    reference: String,
    token: Option<String>,
}

/// Common image-name typos: what users type -> the official repository.
const NAME_SUGGESTIONS: [(&str, &str); 3] = [
    ("postgresql", "postgres"),
    ("mongodb", "mongo"),
    ("nodejs", "node"),
];

/// A typo hint for well-known images, e.g. postgresql -> postgres.
fn name_hint(repository: &str) -> String {
    let short = repository.rsplit('/').next().unwrap_or(repository);
    for (typo, official) in NAME_SUGGESTIONS {
        if short == typo {
            return format!(" (e.g. the official {official} image is '{official}', not '{typo}')");
        }
    }
    String::new()
}

impl RegistryClient {
    pub fn new(image: &ImageReference) -> Result<Self> {
        let scheme =
            if image.registry.starts_with("localhost") || image.registry.starts_with("127.") {
                "http"
            } else {
                "https"
            };
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("mycel/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(RegistryClient {
            http,
            base: format!("{scheme}://{}/v2", image.registry_host()),
            registry: image.registry.clone(),
            repository: image.repository.clone(),
            reference: image.reference().to_string(),
            token: None,
        })
    }

    /// GET with automatic 401 -> token -> retry.
    fn get(&mut self, url: &str, accept: &str) -> Result<reqwest::blocking::Response> {
        for attempt in 0..2 {
            let mut req = self.http.get(url).header("Accept", accept);
            if let Some(t) = &self.token {
                req = req.bearer_auth(t);
            }
            let resp = req.send()?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                let challenge = resp
                    .headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                self.token = Some(self.fetch_token(&challenge)?);
                continue;
            }
            if !resp.status().is_success() {
                return Err(OciError::HttpStatus {
                    url: url.to_string(),
                    status: resp.status().as_u16(),
                });
            }
            return Ok(resp);
        }
        unreachable!()
    }

    /// Parse `WWW-Authenticate: Bearer realm="...",service="...",scope="..."`
    /// and fetch an anonymous pull token.
    fn fetch_token(&self, challenge: &str) -> Result<String> {
        let challenge = challenge.strip_prefix("Bearer ").ok_or_else(|| {
            OciError::Registry(format!("unsupported auth challenge: {challenge}"))
        })?;
        let mut realm = None;
        let mut service = None;
        for part in challenge.split(',') {
            let part = part.trim();
            if let Some((k, v)) = part.split_once('=') {
                let v = v.trim_matches('"');
                match k {
                    "realm" => realm = Some(v.to_string()),
                    "service" => service = Some(v.to_string()),
                    _ => {}
                }
            }
        }
        let realm =
            realm.ok_or_else(|| OciError::Registry("auth challenge without realm".to_string()))?;
        let mut url = format!("{realm}?scope=repository:{}:pull", self.repository);
        if let Some(s) = service {
            url.push_str(&format!("&service={s}"));
        }
        let resp = self.http.get(&url).send()?;
        if !resp.status().is_success() {
            return Err(OciError::Registry(format!(
                "token endpoint {url} -> {}",
                resp.status()
            )));
        }
        let tok: TokenResponse = resp.json()?;
        tok.into_token()
            .ok_or_else(|| OciError::Registry("token endpoint returned no token".to_string()))
    }

    /// Resolve tag/digest to a concrete single-platform image manifest.
    pub fn resolve_image(&mut self, platform: &Platform) -> Result<ResolvedImage> {
        let accept = format!(
            "{MT_OCI_INDEX}, {MT_DOCKER_MANIFEST_LIST}, {MT_OCI_MANIFEST}, {MT_DOCKER_MANIFEST}"
        );
        let url = format!(
            "{}/{}/manifests/{}",
            self.base, self.repository, self.reference
        );
        // 401/404 on a manifest fetch almost always means "no such image":
        // Docker Hub answers 401 for unknown repositories to avoid leaking
        // which private ones exist. Turn it into an actionable message.
        let resp = self.get(&url, &accept).map_err(|e| match e {
            OciError::HttpStatus { status, .. } if status == 401 || status == 404 => {
                OciError::ImageNotFound {
                    image: format!("{}:{}", self.repository, self.reference),
                    registry: self.registry.clone(),
                    hint: name_hint(&self.repository),
                }
            }
            other => other,
        })?;
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = resp.bytes()?;

        match content_type.as_str() {
            MT_OCI_INDEX | MT_DOCKER_MANIFEST_LIST => {
                let index: ImageIndex = serde_json::from_slice(&body)
                    .map_err(|e| OciError::Registry(format!("bad index json: {e}")))?;
                let chosen = index
                    .manifests
                    .iter()
                    .find(|d| {
                        d.platform
                            .as_ref()
                            .is_some_and(|p| p.os == platform.os && p.architecture == platform.arch)
                    })
                    .ok_or_else(|| OciError::NoPlatformMatch {
                        os: platform.os.clone(),
                        arch: platform.arch.clone(),
                    })?;
                // Recurse with the platform-specific digest.
                let mut sub = RegistryClient {
                    http: self.http.clone(),
                    base: self.base.clone(),
                    registry: self.registry.clone(),
                    repository: self.repository.clone(),
                    reference: chosen.digest.clone(),
                    token: self.token.clone(),
                };
                sub.resolve_image(platform)
            }
            MT_OCI_MANIFEST | MT_DOCKER_MANIFEST => {
                let m: ImageManifest = serde_json::from_slice(&body)
                    .map_err(|e| OciError::Registry(format!("bad manifest json: {e}")))?;
                Ok(ResolvedImage {
                    config_digest: m.config.digest,
                    layers: m.layers,
                })
            }
            other => Err(OciError::UnsupportedMediaType(other.to_string())),
        }
    }

    pub fn fetch_config(&mut self, digest: &str) -> Result<ImageConfig> {
        let url = format!("{}/{}/blobs/{digest}", self.base, self.repository);
        let resp = self.get(&url, "application/octet-stream")?;
        let body = resp.bytes()?;
        verify_sha256(&body, digest)?;
        serde_json::from_slice(&body)
            .map_err(|e| OciError::Registry(format!("bad image config: {e}")))
    }

    /// Stream a blob, verifying its sha256 digest as it is read.
    pub fn fetch_blob(&mut self, digest: &str) -> Result<Box<dyn Read + Send>> {
        let url = format!("{}/{}/blobs/{digest}", self.base, self.repository);
        let resp = self.get(&url, "application/octet-stream")?;
        Ok(Box::new(DigestVerifyingReader::new(
            resp,
            digest.to_string(),
        )))
    }
}

fn verify_sha256(data: &[u8], digest: &str) -> Result<()> {
    let expected = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| OciError::Registry(format!("unsupported digest algo: {digest}")))?;
    let actual = hex::encode(Sha256::digest(data));
    if actual != expected {
        return Err(OciError::DigestMismatch {
            expected: digest.to_string(),
        });
    }
    Ok(())
}

/// Wraps a reader and verifies the sha256 digest once EOF is reached.
/// Fails the final read if the content does not match, so corrupted or
/// tampered layers can never be silently ingested.
struct DigestVerifyingReader<R: Read> {
    inner: R,
    hasher: Sha256,
    expected: String,
    finished: bool,
}

impl<R: Read> DigestVerifyingReader<R> {
    fn new(inner: R, expected: String) -> Self {
        DigestVerifyingReader {
            inner,
            hasher: Sha256::new(),
            expected,
            finished: false,
        }
    }
}

impl<R: Read> Read for DigestVerifyingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.hasher.update(&buf[..n]);
        } else if !self.finished {
            self.finished = true;
            let expected_hex = self
                .expected
                .strip_prefix("sha256:")
                .unwrap_or(&self.expected);
            let actual = hex::encode(self.hasher.clone().finalize());
            if actual != expected_hex {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("layer digest mismatch: expected {expected_hex}, got {actual}"),
                ));
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_hints_for_common_typos() {
        assert!(name_hint("library/postgresql").contains("'postgres', not 'postgresql'"));
        assert!(name_hint("library/mongodb").contains("'mongo'"));
        assert!(name_hint("library/nodejs").contains("'node'"));
        assert!(name_hint("library/postgres").is_empty());
        assert!(name_hint("someorg/custom-app").is_empty());
    }

    #[test]
    fn image_not_found_message_reads_well() {
        let err = OciError::ImageNotFound {
            image: "library/postgresql:latest".into(),
            registry: "docker.io".into(),
            hint: name_hint("library/postgresql"),
        };
        let msg = err.to_string();
        assert!(msg.contains("not found on docker.io"), "{msg}");
        assert!(msg.contains("requires authentication"), "{msg}");
        assert!(msg.contains("not 'postgresql'"), "{msg}");
    }
}
