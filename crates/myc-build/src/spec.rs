//! `mycel-build.toml` parsing and validation.
//!
//! The build file is deliberately tiny: a base image, files to add, ordered
//! exec steps, and runtime config overrides. No templating, no stages.

use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// A parsed and validated `mycel-build.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSpec {
    pub build: BuildSection,
    #[serde(default)]
    pub config: Option<ConfigSection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSection {
    /// Base image reference (e.g. `alpine:3.20`). Omitted = build from
    /// scratch (empty rootfs).
    #[serde(default)]
    pub base: Option<String>,
    /// Result name (e.g. `mon-app:1.0`). Overridable with `myc build -t`.
    #[serde(default)]
    pub name: Option<String>,
    /// Files/directories to add. Applied in order, before any run step.
    #[serde(default)]
    pub add: Vec<AddSpec>,
    /// Exec steps (RUN equivalent). Applied in order, after all adds.
    #[serde(default)]
    pub run: Vec<RunStep>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddSpec {
    /// File or directory (recursive), relative to the toml file.
    pub source: String,
    /// Absolute destination path inside the image.
    pub dest: String,
    /// Permission bits for added files (e.g. `0o755`).
    /// Default: preserve the source file's mode. Directories always get 755.
    #[serde(default)]
    pub mode: Option<u32>,
    #[serde(default)]
    pub uid: u32,
    #[serde(default)]
    pub gid: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStep {
    /// argv of the step, e.g. `["/bin/sh", "-c", "apk add curl"]`.
    pub command: Vec<String>,
    /// Extra KEY=VALUE environment for this step only.
    #[serde(default)]
    pub env: Vec<String>,
}

/// Runtime config overrides: base config is the default, every field set
/// here replaces it (env is merged key-by-key instead).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSection {
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    /// Merged over the base env; same key replaces.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
}

/// Normalize a `dest` path: must be absolute, no `.`/`..` components.
/// Returns `/a/b/c` form; `/` is allowed (directory adds at the root).
pub fn normalize_dest(raw: &str) -> Result<String, String> {
    if !raw.starts_with('/') {
        return Err(format!("dest must be an absolute path, got '{raw}'"));
    }
    let mut parts: Vec<&str> = Vec::new();
    for comp in raw.split('/') {
        match comp {
            "" | "." => {}
            ".." => return Err(format!("dest must not contain '..': '{raw}'")),
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(format!("/{}", parts.join("/")))
    }
}

fn validate_env(list: &[String], where_: &str) -> Result<(), String> {
    for kv in list {
        if !kv.contains('=') {
            return Err(format!("{where_}: env entry '{kv}' is not KEY=VALUE"));
        }
    }
    Ok(())
}

impl BuildSpec {
    pub fn from_toml_str(text: &str) -> Result<BuildSpec, String> {
        let spec: BuildSpec = toml::from_str(text).map_err(|e| e.to_string())?;
        spec.validate()?;
        Ok(spec)
    }

    /// Load from a file. Returns the spec and the context directory
    /// (the toml's parent) against which `add.source` paths are resolved.
    pub fn load(path: &Path) -> Result<(BuildSpec, PathBuf), String> {
        let text =
            fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let spec = Self::from_toml_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let ctx = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok((spec, ctx))
    }

    fn validate(&self) -> Result<(), String> {
        for (i, add) in self.build.add.iter().enumerate() {
            if add.source.is_empty() {
                return Err(format!("build.add[{i}]: source is empty"));
            }
            normalize_dest(&add.dest).map_err(|e| format!("build.add[{i}]: {e}"))?;
            if let Some(mode) = add.mode {
                if mode > 0o7777 {
                    return Err(format!("build.add[{i}]: mode {mode:o} out of range"));
                }
            }
        }
        for (i, step) in self.build.run.iter().enumerate() {
            if step.command.is_empty() {
                return Err(format!("build.run[{i}]: command is empty"));
            }
            validate_env(&step.env, &format!("build.run[{i}]"))?;
        }
        if let Some(cfg) = &self.config {
            if let Some(env) = &cfg.env {
                validate_env(env, "config")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
[build]
base = "alpine:3.20"
name = "mon-app:1.0"

[[build.add]]
source = "./dist/mon-app"
dest = "/app/mon-app"
mode = 0o755
uid = 0
gid = 0

[[build.run]]
command = ["/bin/sh", "-c", "echo hi"]
env = ["http_proxy="]

[config]
entrypoint = ["/app/mon-app"]
cmd = ["--serve"]
env = ["PORT=8080"]
workdir = "/app"
"#;

    #[test]
    fn parses_full_example() {
        let spec = BuildSpec::from_toml_str(FULL).unwrap();
        assert_eq!(spec.build.base.as_deref(), Some("alpine:3.20"));
        assert_eq!(spec.build.name.as_deref(), Some("mon-app:1.0"));
        assert_eq!(spec.build.add.len(), 1);
        assert_eq!(spec.build.add[0].mode, Some(0o755));
        assert_eq!(spec.build.run.len(), 1);
        assert_eq!(spec.build.run[0].env, vec!["http_proxy=".to_string()]);
        let cfg = spec.config.unwrap();
        assert_eq!(cfg.entrypoint.unwrap(), vec!["/app/mon-app"]);
        assert_eq!(cfg.workdir.as_deref(), Some("/app"));
    }

    #[test]
    fn minimal_scratch_build() {
        let spec = BuildSpec::from_toml_str("[build]\nname = \"x:1\"\n").unwrap();
        assert!(spec.build.base.is_none());
        assert!(spec.build.add.is_empty());
        assert!(spec.build.run.is_empty());
    }

    #[test]
    fn rejects_relative_dest() {
        let toml = r#"
[build]
name = "x:1"
[[build.add]]
source = "f"
dest = "app/f"
"#;
        let err = BuildSpec::from_toml_str(toml).unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn rejects_dotdot_dest() {
        let toml = r#"
[build]
name = "x:1"
[[build.add]]
source = "f"
dest = "/app/../../etc/passwd"
"#;
        let err = BuildSpec::from_toml_str(toml).unwrap_err();
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn rejects_empty_run_command() {
        let toml = "[build]\nname = \"x:1\"\n[[build.run]]\ncommand = []\n";
        let err = BuildSpec::from_toml_str(toml).unwrap_err();
        assert!(err.contains("command is empty"), "{err}");
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = BuildSpec::from_toml_str("[build]\nnmae = \"typo:1\"\n").unwrap_err();
        assert!(err.contains("nmae"), "{err}");
    }

    #[test]
    fn rejects_bad_env() {
        let toml = "[build]\nname = \"x:1\"\n[config]\nenv = [\"NOEQUALS\"]\n";
        let err = BuildSpec::from_toml_str(toml).unwrap_err();
        assert!(err.contains("KEY=VALUE"), "{err}");
    }

    #[test]
    fn normalizes_dest() {
        assert_eq!(normalize_dest("/a//b/./c").unwrap(), "/a/b/c");
        assert_eq!(normalize_dest("/").unwrap(), "/");
        assert!(normalize_dest("x/y").is_err());
    }
}
