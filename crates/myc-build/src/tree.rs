//! The in-memory file tree a build mutates, plus merge semantics.
//!
//! Shadowing follows the same rules as OCI layer application: inserting a
//! non-directory over a directory removes the whole subtree.

use crate::spec::ConfigSection;
use myc_manifest::{Entry, EntryKind, RuntimeConfig};
use std::collections::BTreeMap;

/// path -> entry, sorted (canonical manifest order for free).
pub type FileTree = BTreeMap<String, Entry>;

pub fn from_entries(entries: &[Entry]) -> FileTree {
    entries
        .iter()
        .map(|e| (e.path.clone(), e.clone()))
        .collect()
}

/// Insert an entry, applying shadowing: a non-directory replacing a
/// directory removes everything under it.
pub fn insert(tree: &mut FileTree, entry: Entry) {
    if let Some(prev) = tree.get(&entry.path) {
        if prev.kind == EntryKind::Dir && entry.kind != EntryKind::Dir {
            let prefix = format!("{}/", entry.path);
            tree.retain(|p, _| !p.starts_with(&prefix));
        }
    }
    tree.insert(entry.path.clone(), entry);
}

/// Ensure every ancestor of `path` exists as a directory entry (mode 755,
/// root-owned). An ancestor that exists as a non-directory is converted.
pub fn ensure_parents(tree: &mut FileTree, path: &str) {
    let mut ancestor = String::new();
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        ancestor.push('/');
        ancestor.push_str(part);
        let is_dir = tree
            .get(&ancestor)
            .map(|e| e.kind == EntryKind::Dir)
            .unwrap_or(false);
        if !is_dir {
            tree.insert(
                ancestor.clone(),
                Entry {
                    path: ancestor.clone(),
                    kind: EntryKind::Dir,
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    size: 0,
                    blake3: None,
                    target: None,
                    device: None,
                },
            );
        }
    }
}

/// Merge env lists: `base` order is kept, `over` replaces same-key values
/// and appends new keys.
pub fn merge_env(base: &[String], over: &[String]) -> Vec<String> {
    let key = |kv: &str| kv.split('=').next().unwrap_or_default().to_string();
    let mut out: Vec<String> = base.to_vec();
    for kv in over {
        let k = key(kv);
        match out.iter_mut().find(|e| key(e) == k) {
            Some(slot) => *slot = kv.clone(),
            None => out.push(kv.clone()),
        }
    }
    out
}

/// Base config as default, overridden field-by-field (env merges key-wise).
pub fn merge_config(base: Option<&RuntimeConfig>, over: Option<&ConfigSection>) -> RuntimeConfig {
    let mut cfg = base.cloned().unwrap_or_default();
    let Some(over) = over else { return cfg };
    if let Some(entrypoint) = &over.entrypoint {
        cfg.entrypoint = entrypoint.clone();
    }
    if let Some(cmd) = &over.cmd {
        cfg.cmd = cmd.clone();
    }
    if let Some(env) = &over.env {
        cfg.env = merge_env(&cfg.env, env);
    }
    if let Some(workdir) = &over.workdir {
        cfg.workdir = workdir.clone();
    }
    if let Some(user) = &over.user {
        cfg.user = user.clone();
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(path: &str) -> Entry {
        Entry {
            path: path.into(),
            kind: EntryKind::Dir,
            mode: 0o755,
            uid: 0,
            gid: 0,
            size: 0,
            blake3: None,
            target: None,
            device: None,
        }
    }

    fn file(path: &str) -> Entry {
        Entry {
            path: path.into(),
            kind: EntryKind::File,
            mode: 0o644,
            uid: 0,
            gid: 0,
            size: 1,
            blake3: Some("h".into()),
            target: None,
            device: None,
        }
    }

    #[test]
    fn file_over_dir_shadows_subtree() {
        let mut tree = FileTree::new();
        insert(&mut tree, dir("/app"));
        insert(&mut tree, file("/app/a"));
        insert(&mut tree, file("/app/b"));
        insert(&mut tree, file("/app"));
        assert_eq!(tree.len(), 1);
        assert_eq!(tree["/app"].kind, EntryKind::File);
    }

    #[test]
    fn dir_over_file_keeps_siblings() {
        let mut tree = FileTree::new();
        insert(&mut tree, file("/app"));
        insert(&mut tree, dir("/app"));
        insert(&mut tree, file("/app/a"));
        assert_eq!(tree["/app"].kind, EntryKind::Dir);
        assert!(tree.contains_key("/app/a"));
    }

    #[test]
    fn parents_created_and_converted() {
        let mut tree = FileTree::new();
        insert(&mut tree, file("/a"));
        ensure_parents(&mut tree, "/a/b/c");
        assert_eq!(tree["/a"].kind, EntryKind::Dir);
        assert_eq!(tree["/a/b"].kind, EntryKind::Dir);
        assert_eq!(tree["/a/b"].mode, 0o755);
        assert!(!tree.contains_key("/a/b/c"));
    }

    #[test]
    fn env_merge_replaces_same_key() {
        let base = vec!["PATH=/bin".to_string(), "LANG=C".to_string()];
        let over = vec!["PATH=/app:/bin".to_string(), "PORT=8080".to_string()];
        assert_eq!(
            merge_env(&base, &over),
            vec!["PATH=/app:/bin", "LANG=C", "PORT=8080"]
        );
    }

    #[test]
    fn config_merge_field_by_field() {
        let base = RuntimeConfig {
            env: vec!["A=1".into()],
            entrypoint: vec!["/init".into()],
            cmd: vec!["serve".into()],
            workdir: "/".into(),
            user: "".into(),
            exposed_ports: Vec::new(),
            volumes: Vec::new(),
        };
        let over = ConfigSection {
            entrypoint: None,
            cmd: Some(vec!["--debug".into()]),
            env: Some(vec!["A=2".into(), "B=3".into()]),
            workdir: Some("/app".into()),
            user: None,
        };
        let merged = merge_config(Some(&base), Some(&over));
        assert_eq!(merged.entrypoint, vec!["/init"]); // untouched
        assert_eq!(merged.cmd, vec!["--debug"]); // replaced
        assert_eq!(merged.env, vec!["A=2", "B=3"]); // merged
        assert_eq!(merged.workdir, "/app");
    }
}
