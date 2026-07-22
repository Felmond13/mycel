//! The Mycel project file (`mycel.toml`) and local multi-service runner.
//!
//! ```toml
//! [project]
//! name = "shop"
//!
//! [services.db]
//! image = "postgres:16"
//! env = ["POSTGRES_PASSWORD=dev"]
//!
//! [services.api]
//! image = "ghcr.io/acme/api:1.2"
//! command = ["/app/serve", "--port", "8080"]
//! depends_on = ["db"]
//! binds = ["./data:/var/lib/app"]
//! ```
//!
//! `up` resolves every image (ingesting from the registry when missing),
//! starts services in dependency order and runs them in the foreground.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ComposeError {
    #[error("cannot read {0}: {1}")]
    Read(PathBuf, std::io::Error),
    #[error("invalid mycel.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("service '{0}' depends on unknown service '{1}'")]
    UnknownDependency(String, String),
    #[error("dependency cycle involving service '{0}'")]
    Cycle(String),
    #[error("invalid bind '{0}' (expected HOST:CONTAINER[:ro])")]
    BadBind(String),
    #[error(
        "invalid [network] port '{0}' (expected \"PORT\" or \"HOST:CONTAINER\", each 1-65535)"
    )]
    BadPort(String),
    #[error("[network] ports only make sense in pod mode — add `mode = \"pod\"` to the [network] section, or remove `ports`")]
    PortsWithoutPod,
    #[error("[network] publishes host port {0} twice")]
    DuplicatePort(u16),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectFile {
    #[serde(default)]
    pub project: ProjectMeta,
    /// Optional `[network]` section: `mode = "pod"` puts the whole stack in
    /// one private network namespace (services see each other on localhost,
    /// only `ports` are published to the host). Absent = host networking.
    #[serde(default, skip_serializing_if = "NetworkConfig::is_default")]
    pub network: NetworkConfig,
    pub services: BTreeMap<String, Service>,
}

/// How the stack's services join the network.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// Every service shares the host network (the historical behavior).
    #[default]
    Host,
    /// The stack lives in its own private network: one namespace shared by
    /// all services, internal traffic on localhost, only [`NetworkConfig::ports`]
    /// reachable from the host.
    Pod,
}

/// The `[network]` section of `mycel.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default)]
    pub mode: NetworkMode,
    /// Ports published from the pod to the host, as `"PORT"` (same number
    /// on both sides) or `"HOST:CONTAINER"`. Everything else stays internal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<String>,
}

impl NetworkConfig {
    fn is_default(&self) -> bool {
        *self == NetworkConfig::default()
    }

    /// Parse and validate the published ports into `(host, container)`
    /// pairs. Rejects malformed specs and duplicate host ports.
    pub fn parsed_ports(&self) -> Result<Vec<(u16, u16)>, ComposeError> {
        let mut out = Vec::with_capacity(self.ports.len());
        let mut seen = HashSet::new();
        for spec in &self.ports {
            let pair = parse_pod_port(spec)?;
            if !seen.insert(pair.0) {
                return Err(ComposeError::DuplicatePort(pair.0));
            }
            out.push(pair);
        }
        Ok(out)
    }

    /// Structural validation of the section itself.
    pub fn validate(&self) -> Result<(), ComposeError> {
        if self.mode == NetworkMode::Host && !self.ports.is_empty() {
            return Err(ComposeError::PortsWithoutPod);
        }
        self.parsed_ports().map(|_| ())
    }
}

/// Parse one `[network] ports` entry: `"8080"` or `"8080:80"` (host first).
pub fn parse_pod_port(spec: &str) -> Result<(u16, u16), ComposeError> {
    let bad = || ComposeError::BadPort(spec.to_string());
    let port = |s: &str| s.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(bad);
    match spec.split_once(':') {
        Some((h, c)) => Ok((port(h)?, port(c)?)),
        None => {
            let p = port(spec)?;
            Ok((p, p))
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMeta {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Image reference (`postgres:16`) or a stored manifest id/ref.
    pub image: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    /// `SOURCE:CONTAINER[:ro]` mounts: SOURCE is a host path (relative to
    /// the project file) or a named volume (`dbdata:/var/lib/postgresql`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

impl ProjectFile {
    pub fn load(path: &Path) -> Result<Self, ComposeError> {
        let text = std::fs::read_to_string(path).map_err(|e| ComposeError::Read(path.into(), e))?;
        Self::parse(&text)
    }

    /// Parse and validate a `mycel.toml` document from a string.
    pub fn parse(text: &str) -> Result<Self, ComposeError> {
        let file: ProjectFile = toml::from_str(text)?;
        file.validate()?;
        Ok(file)
    }

    /// Serialize back to canonical `mycel.toml` text.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("project file is always serializable")
    }

    /// Full structural validation (dependencies exist, no cycles, binds
    /// parse, the [network] section is coherent).
    pub fn validate(&self) -> Result<(), ComposeError> {
        self.network.validate()?;
        for (name, svc) in &self.services {
            for dep in &svc.depends_on {
                if !self.services.contains_key(dep) {
                    return Err(ComposeError::UnknownDependency(name.clone(), dep.clone()));
                }
            }
            for bind in &svc.binds {
                parse_mount(bind, Path::new("."))?;
            }
        }
        self.start_order()?;
        Ok(())
    }

    /// Topological start order (dependencies first). Deterministic: services
    /// are iterated in name order.
    pub fn start_order(&self) -> Result<Vec<String>, ComposeError> {
        let mut order = Vec::new();
        let mut done: HashSet<String> = HashSet::new();
        let mut visiting: HashSet<String> = HashSet::new();

        fn visit(
            name: &str,
            services: &BTreeMap<String, Service>,
            done: &mut HashSet<String>,
            visiting: &mut HashSet<String>,
            order: &mut Vec<String>,
        ) -> Result<(), ComposeError> {
            if done.contains(name) {
                return Ok(());
            }
            if !visiting.insert(name.to_string()) {
                return Err(ComposeError::Cycle(name.to_string()));
            }
            for dep in &services[name].depends_on {
                visit(dep, services, done, visiting, order)?;
            }
            visiting.remove(name);
            done.insert(name.to_string());
            order.push(name.to_string());
            Ok(())
        }

        for name in self.services.keys() {
            visit(name, &self.services, &mut done, &mut visiting, &mut order)?;
        }
        Ok(order)
    }
}

/// A parsed `-v` / `binds` entry: either a host-path bind mount or a named
/// volume managed by the store (`myvolume:/data`, Docker-style — a source
/// with no `/`, not starting with `.` or `~`, is a volume name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    Bind {
        host: PathBuf,
        container: String,
        read_only: bool,
    },
    Volume {
        name: String,
        container: String,
        read_only: bool,
    },
}

/// Parse `SOURCE:CONTAINER[:ro]` where SOURCE is a host path (absolute or
/// relative to `base`) or a named volume.
pub fn parse_mount(spec: &str, base: &Path) -> Result<Mount, ComposeError> {
    let parts: Vec<&str> = spec.split(':').collect();
    let (source, container, read_only) = match parts.as_slice() {
        [s, c] => (*s, *c, false),
        [s, c, "ro"] => (*s, *c, true),
        _ => return Err(ComposeError::BadBind(spec.to_string())),
    };
    if !container.starts_with('/') || source.is_empty() {
        return Err(ComposeError::BadBind(spec.to_string()));
    }
    let container = container.to_string();
    let looks_like_path =
        source.contains('/') || source.starts_with('.') || source.starts_with('~');
    if !looks_like_path {
        return Ok(Mount::Volume {
            name: source.to_string(),
            container,
            read_only,
        });
    }
    let host = if Path::new(source).is_absolute() {
        PathBuf::from(source)
    } else {
        base.join(source)
    };
    Ok(Mount::Bind {
        host,
        container,
        read_only,
    })
}

/// Parse `HOST:CONTAINER[:ro]`, resolving HOST relative to `base`.
/// Named-volume sources are rejected here — use [`parse_mount`] where
/// volumes are supported.
pub fn parse_bind(spec: &str, base: &Path) -> Result<(PathBuf, String, bool), ComposeError> {
    match parse_mount(spec, base)? {
        Mount::Bind {
            host,
            container,
            read_only,
        } => Ok((host, container, read_only)),
        Mount::Volume { .. } => Err(ComposeError::BadBind(spec.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_project(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mycel.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
        (dir, path)
    }

    #[test]
    fn parse_and_order() {
        let (_d, path) = write_project(
            r#"
[project]
name = "shop"

[services.db]
image = "postgres:16"
env = ["POSTGRES_PASSWORD=dev"]

[services.cache]
image = "redis:7"

[services.api]
image = "acme/api:1"
depends_on = ["db", "cache"]
"#,
        );
        let project = ProjectFile::load(&path).unwrap();
        let order = project.start_order().unwrap();
        let pos = |s: &str| order.iter().position(|x| x == s).unwrap();
        assert!(pos("db") < pos("api"));
        assert!(pos("cache") < pos("api"));
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn rejects_unknown_dependency() {
        let (_d, path) = write_project(
            r#"
[services.api]
image = "a:1"
depends_on = ["ghost"]
"#,
        );
        assert!(matches!(
            ProjectFile::load(&path),
            Err(ComposeError::UnknownDependency(_, _))
        ));
    }

    #[test]
    fn rejects_cycles() {
        let (_d, path) = write_project(
            r#"
[services.a]
image = "a:1"
depends_on = ["b"]

[services.b]
image = "b:1"
depends_on = ["a"]
"#,
        );
        assert!(matches!(
            ProjectFile::load(&path),
            Err(ComposeError::Cycle(_))
        ));
    }

    #[test]
    fn bind_parsing() {
        let base = Path::new("/proj");
        let (h, c, ro) = parse_bind("./data:/var/lib/app", base).unwrap();
        assert_eq!(h, PathBuf::from("/proj/./data"));
        assert_eq!(c, "/var/lib/app");
        assert!(!ro);
        let (_, _, ro) = parse_bind("/abs:/x:ro", base).unwrap();
        assert!(ro);
        assert!(parse_bind("nocontainer", base).is_err());
        assert!(parse_bind("./x:relative", base).is_err());
        // Named-volume sources belong to parse_mount, not parse_bind.
        assert!(parse_bind("myvol:/data", base).is_err());
    }

    #[test]
    fn network_section_defaults_to_host() {
        let (_d, path) = write_project("[services.web]\nimage = \"nginx:1\"\n");
        let project = ProjectFile::load(&path).unwrap();
        assert_eq!(project.network.mode, NetworkMode::Host);
        assert!(project.network.ports.is_empty());
        // Absent section stays absent when serialized back.
        assert!(!project.to_toml().contains("[network]"));
    }

    #[test]
    fn network_pod_with_ports_parses_and_roundtrips() {
        let (_d, path) = write_project(
            r#"
[network]
mode = "pod"
ports = ["8080", "15432:5432"]

[services.web]
image = "nginx:1"
"#,
        );
        let project = ProjectFile::load(&path).unwrap();
        assert_eq!(project.network.mode, NetworkMode::Pod);
        assert_eq!(
            project.network.parsed_ports().unwrap(),
            vec![(8080, 8080), (15432, 5432)]
        );
        let text = project.to_toml();
        assert!(text.contains("[network]"), "toml was: {text}");
        let reparsed = ProjectFile::parse(&text).unwrap();
        assert_eq!(reparsed.network, project.network);
    }

    #[test]
    fn network_rejects_bad_input() {
        // Unknown mode: toml's enum error names the accepted values.
        let err =
            ProjectFile::parse("[network]\nmode = \"bridge\"\n\n[services.a]\nimage = \"x:1\"\n")
                .unwrap_err()
                .to_string();
        assert!(err.contains("host") && err.contains("pod"), "error: {err}");

        // Malformed / out-of-range ports.
        for bad in ["", "abc", "0", "70000", "8080:", ":80", "1:2:3"] {
            let err = parse_pod_port(bad).unwrap_err().to_string();
            assert!(err.contains("PORT"), "error for '{bad}': {err}");
        }

        // Ports without pod mode are refused with an actionable message.
        let err =
            ProjectFile::parse("[network]\nports = [\"8080\"]\n\n[services.a]\nimage = \"x:1\"\n")
                .unwrap_err()
                .to_string();
        assert!(err.contains("pod"), "error: {err}");

        // Duplicate host ports are refused.
        let err = ProjectFile::parse(
            "[network]\nmode = \"pod\"\nports = [\"8080:80\", \"8080:81\"]\n\n[services.a]\nimage = \"x:1\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("8080") && err.contains("twice"),
            "error: {err}"
        );
    }

    #[test]
    fn mount_parsing_distinguishes_volumes_from_paths() {
        let base = Path::new("/proj");
        assert_eq!(
            parse_mount("dbdata:/var/lib/postgresql:ro", base).unwrap(),
            Mount::Volume {
                name: "dbdata".into(),
                container: "/var/lib/postgresql".into(),
                read_only: true,
            }
        );
        // Anything path-like stays a bind: absolute, ./relative, ~, or
        // containing a slash.
        assert_eq!(
            parse_mount("./data:/data", base).unwrap(),
            Mount::Bind {
                host: PathBuf::from("/proj/./data"),
                container: "/data".into(),
                read_only: false,
            }
        );
        assert!(matches!(
            parse_mount("/abs:/data", base).unwrap(),
            Mount::Bind { .. }
        ));
        assert!(matches!(
            parse_mount("sub/dir:/data", base).unwrap(),
            Mount::Bind { .. }
        ));
        assert!(parse_mount(":/data", base).is_err());
        assert!(parse_mount("vol:data", base).is_err());
    }
}
