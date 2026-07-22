//! Docker-style image reference parsing.
//!
//! Follows the same defaulting rules as the Docker CLI:
//! `alpine` -> `docker.io/library/alpine:latest`
//! `ghcr.io/org/app:v1` -> unchanged.

use crate::OciError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    pub registry: String,
    pub repository: String,
    pub tag: String,
    /// Digest pin (`@sha256:...`), takes precedence over tag when present.
    pub digest: Option<String>,
    /// What the user actually typed, for display.
    pub original: String,
}

impl ImageReference {
    pub fn parse(input: &str) -> Result<Self, OciError> {
        let original = input.to_string();
        if input.is_empty() {
            return Err(OciError::BadReference(original));
        }

        // Split off digest first.
        let (rest, digest) = match input.split_once('@') {
            Some((r, d)) => {
                if !d.starts_with("sha256:") {
                    return Err(OciError::BadReference(original));
                }
                (r, Some(d.to_string()))
            }
            None => (input, None),
        };

        // Determine whether the first component is a registry host:
        // it is if it contains '.', ':' or is "localhost".
        let (registry, remainder) = match rest.split_once('/') {
            Some((first, rem))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), rem.to_string())
            }
            _ => ("docker.io".to_string(), rest.to_string()),
        };

        // Split tag from repository (careful: registry ports were handled above).
        let (repository, tag) = match remainder.rsplit_once(':') {
            Some((repo, t)) if !t.contains('/') => (repo.to_string(), t.to_string()),
            _ => (remainder.clone(), "latest".to_string()),
        };

        if repository.is_empty() {
            return Err(OciError::BadReference(original));
        }

        // Docker Hub official images live under `library/`.
        let repository = if registry == "docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };

        Ok(ImageReference {
            registry,
            repository,
            tag,
            digest,
            original,
        })
    }

    /// Full canonical name, e.g. `docker.io/library/alpine:3.20`.
    pub fn canonical_name(&self) -> String {
        match &self.digest {
            Some(d) => format!("{}/{}@{}", self.registry, self.repository, d),
            None => format!("{}/{}:{}", self.registry, self.repository, self.tag),
        }
    }

    /// The name as typed by the user (normalized with tag), e.g. `alpine:3.20`.
    pub fn short_name(&self) -> String {
        if self.original.contains(':') || self.original.contains('@') {
            self.original.clone()
        } else {
            format!("{}:{}", self.original, self.tag)
        }
    }

    /// What to ask the registry for: digest if pinned, else tag.
    pub fn reference(&self) -> &str {
        self.digest.as_deref().unwrap_or(&self.tag)
    }

    /// API endpoint host. Docker Hub's registry lives at registry-1.docker.io.
    pub fn registry_host(&self) -> &str {
        if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name() {
        let r = ImageReference::parse("alpine").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/alpine");
        assert_eq!(r.tag, "latest");
        assert_eq!(r.canonical_name(), "docker.io/library/alpine:latest");
        assert_eq!(r.registry_host(), "registry-1.docker.io");
    }

    #[test]
    fn name_with_tag() {
        let r = ImageReference::parse("postgres:16").unwrap();
        assert_eq!(r.repository, "library/postgres");
        assert_eq!(r.tag, "16");
        assert_eq!(r.short_name(), "postgres:16");
    }

    #[test]
    fn org_image() {
        let r = ImageReference::parse("grafana/grafana:10.0.0").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "grafana/grafana");
    }

    #[test]
    fn other_registry() {
        let r = ImageReference::parse("ghcr.io/org/app:v1").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "org/app");
        assert_eq!(r.registry_host(), "ghcr.io");
    }

    #[test]
    fn registry_with_port() {
        let r = ImageReference::parse("localhost:5000/app").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "app");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn digest_pin() {
        let r = ImageReference::parse(
            "alpine@sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )
        .unwrap();
        assert!(r.digest.is_some());
        assert_eq!(r.reference(), r.digest.as_deref().unwrap());
    }

    #[test]
    fn invalid() {
        assert!(ImageReference::parse("").is_err());
        assert!(ImageReference::parse("alpine@md5:abc").is_err());
    }
}
