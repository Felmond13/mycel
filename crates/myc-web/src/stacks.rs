//! Stack (multi-service project) management for the web dashboard.
//!
//! A stack is a `mycel.toml` project owned by the UI, stored under
//! `<store>/stacks/<name>/mycel.toml` and reusing `myc-compose` for parsing,
//! validation and dependency ordering. Stack names are strictly validated so
//! they can never traverse out of the stacks directory.

use myc_compose::{ProjectFile, ProjectMeta, Service};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `[a-z0-9][a-z0-9_-]{0,31}` — safe as a directory name and readable.
pub fn valid_stack_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 32
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Same shape for service names inside a stack.
pub fn valid_service_name(name: &str) -> bool {
    valid_stack_name(name)
}

pub fn stacks_dir(store_root: &Path) -> PathBuf {
    store_root.join("stacks")
}

pub fn stack_dir(store_root: &Path, name: &str) -> PathBuf {
    stacks_dir(store_root).join(name)
}

pub fn stack_file(store_root: &Path, name: &str) -> PathBuf {
    stack_dir(store_root, name).join("mycel.toml")
}

/// All stack names that currently have a `mycel.toml` on disk.
pub fn list_stacks(store_root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir(stacks_dir(store_root)) else {
        return names;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if valid_stack_name(&name) && stack_file(store_root, &name).is_file() {
            names.push(name);
        }
    }
    names.sort();
    names
}

pub fn load_stack(store_root: &Path, name: &str) -> Result<(ProjectFile, String), String> {
    let path = stack_file(store_root, name);
    let text = std::fs::read_to_string(&path).map_err(|_| format!("stack '{name}' not found"))?;
    let project = ProjectFile::parse(&text).map_err(|e| e.to_string())?;
    Ok((project, text))
}

/// Validate and persist a stack; returns the canonical TOML that was written.
pub fn save_stack(store_root: &Path, name: &str, project: &ProjectFile) -> Result<String, String> {
    project.validate().map_err(|e| e.to_string())?;
    for svc_name in project.services.keys() {
        if !valid_service_name(svc_name) {
            return Err(format!(
                "invalid service name '{svc_name}' (use lowercase letters, digits, - and _)"
            ));
        }
    }
    let dir = stack_dir(store_root, name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create stack dir: {e}"))?;
    let text = project.to_toml();
    std::fs::write(stack_file(store_root, name), &text)
        .map_err(|e| format!("cannot write stack file: {e}"))?;
    Ok(text)
}

pub fn delete_stack(store_root: &Path, name: &str) -> Result<(), String> {
    let dir = stack_dir(store_root, name);
    if !dir.exists() {
        return Err(format!("stack '{name}' not found"));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("cannot delete stack: {e}"))
}

/// Build a validated `ProjectFile` from the structured JSON the visual stack
/// builder sends: `{ "web": {"image": "...", "command": [...], ...}, ... }`.
pub fn project_from_services(
    name: &str,
    services: BTreeMap<String, ServiceInput>,
) -> Result<ProjectFile, String> {
    if services.is_empty() {
        return Err("a stack needs at least one service".to_string());
    }
    let services: BTreeMap<String, Service> = services
        .into_iter()
        .map(|(svc_name, input)| {
            let service = Service {
                image: input.image.trim().to_string(),
                command: input.command,
                env: input.env,
                binds: Vec::new(),
                depends_on: input.depends_on,
                workdir: input.workdir.filter(|w| !w.is_empty()),
                hostname: None,
            };
            (svc_name, service)
        })
        .collect();
    for (svc_name, svc) in &services {
        if svc.image.is_empty() {
            return Err(format!("service '{svc_name}' has no image"));
        }
        for kv in &svc.env {
            if !kv.contains('=') {
                return Err(format!(
                    "service '{svc_name}': env entry '{kv}' is not KEY=VALUE"
                ));
            }
        }
    }
    let project = ProjectFile {
        project: ProjectMeta {
            name: name.to_string(),
        },
        // The visual builder has no network form yet: stacks it creates use
        // host networking; pod mode is set through the raw-TOML editor.
        network: myc_compose::NetworkConfig::default(),
        services,
    };
    project.validate().map_err(|e| e.to_string())?;
    Ok(project)
}

/// One service as sent by the visual builder.
#[derive(Debug, serde::Deserialize)]
pub struct ServiceInput {
    pub image: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub workdir: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(image: &str, deps: &[&str]) -> ServiceInput {
        ServiceInput {
            image: image.to_string(),
            command: Vec::new(),
            env: Vec::new(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            workdir: None,
        }
    }

    #[test]
    fn name_validation() {
        assert!(valid_stack_name("my-blog"));
        assert!(valid_stack_name("a"));
        assert!(valid_stack_name("stack_2"));
        assert!(!valid_stack_name(""));
        assert!(!valid_stack_name("-leading"));
        assert!(!valid_stack_name("Has-Caps"));
        assert!(!valid_stack_name("dot.dot"));
        assert!(!valid_stack_name("slash/attack"));
        assert!(!valid_stack_name("..")); // path traversal shape
        assert!(!valid_stack_name(&"x".repeat(33)));
    }

    #[test]
    fn build_validate_and_roundtrip() {
        let mut services = BTreeMap::new();
        services.insert("db".to_string(), input("postgres:16", &[]));
        services.insert("api".to_string(), input("acme/api:1", &["db"]));
        let project = project_from_services("shop", services).unwrap();
        assert_eq!(project.project.name, "shop");

        // Serialize -> parse -> same order guarantees.
        let text = project.to_toml();
        let parsed = ProjectFile::parse(&text).unwrap();
        let order = parsed.start_order().unwrap();
        assert_eq!(order, vec!["db".to_string(), "api".to_string()]);
    }

    #[test]
    fn rejects_bad_input() {
        // missing image
        let mut services = BTreeMap::new();
        services.insert("web".to_string(), input("", &[]));
        assert!(project_from_services("s", services).is_err());

        // env without '='
        let mut services = BTreeMap::new();
        let mut bad = input("nginx:latest", &[]);
        bad.env = vec!["NOEQUALS".to_string()];
        services.insert("web".to_string(), bad);
        assert!(project_from_services("s", services).is_err());

        // unknown dependency
        let mut services = BTreeMap::new();
        services.insert("web".to_string(), input("nginx:latest", &["ghost"]));
        assert!(project_from_services("s", services).is_err());

        // dependency cycle
        let mut services = BTreeMap::new();
        services.insert("a".to_string(), input("x:1", &["b"]));
        services.insert("b".to_string(), input("x:1", &["a"]));
        assert!(project_from_services("s", services).is_err());

        // no services at all
        assert!(project_from_services("s", BTreeMap::new()).is_err());
    }

    #[test]
    fn save_load_delete_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(list_stacks(root).is_empty());

        let mut services = BTreeMap::new();
        services.insert("web".to_string(), input("nginx:latest", &[]));
        let project = project_from_services("site", services).unwrap();
        let written = save_stack(root, "site", &project).unwrap();
        assert!(written.contains("[services.web]"));

        assert_eq!(list_stacks(root), vec!["site".to_string()]);
        let (loaded, text) = load_stack(root, "site").unwrap();
        assert_eq!(text, written);
        assert_eq!(loaded.services["web"].image, "nginx:latest");

        delete_stack(root, "site").unwrap();
        assert!(list_stacks(root).is_empty());
        assert!(load_stack(root, "site").is_err());
        assert!(delete_stack(root, "site").is_err());
    }
}
