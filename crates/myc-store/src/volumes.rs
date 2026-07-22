//! Named volumes: persistent data directories managed by the store.
//!
//! A volume is a plain directory under `<store>/volumes/<name>/data` plus a
//! small metadata file. Containers bind-mount the `data` directory; the
//! store never interprets its contents. Deleting a volume deletes the data.
//!
//! ```text
//! .mycel/volumes/<name>/
//!   volume.json     {"name", "created_at", "created_for"?}
//!   data/           what the container sees
//! ```

use crate::{io_err, Result, Store, StoreError};
use std::fs;
use std::path::{Path, PathBuf};

/// Metadata stored next to each volume's data directory.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VolumeMeta {
    pub name: String,
    /// Unix seconds.
    pub created_at: u64,
    /// Image or app the volume was first created for (display only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_for: Option<String>,
}

/// A volume with its metadata and measured size.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VolumeInfo {
    pub name: String,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_for: Option<String>,
    /// Total bytes under `data/` (regular files, after symlink exclusion).
    pub size_bytes: u64,
    /// Absolute host path of the data directory.
    pub path: PathBuf,
}

/// Valid volume names: 1-64 chars of `[a-z0-9._-]`, starting with `[a-z0-9]`.
/// Lowercase-only keeps names portable to case-insensitive filesystems.
pub fn valid_volume_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 64
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

fn check_name(name: &str) -> Result<()> {
    if valid_volume_name(name) {
        Ok(())
    } else {
        Err(StoreError::InvalidVolumeName(name.to_string()))
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Recursive size of everything under `dir` (files only, symlinks skipped).
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                total += dir_size(&entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

impl Store {
    fn volumes_root(&self) -> PathBuf {
        self.root().join("volumes")
    }

    fn volume_dir(&self, name: &str) -> PathBuf {
        self.volumes_root().join(name)
    }

    /// Host path of a volume's data directory (what containers mount).
    pub fn volume_data_path(&self, name: &str) -> Result<PathBuf> {
        check_name(name)?;
        Ok(self.volume_dir(name).join("data"))
    }

    /// Create `name` if it does not exist yet (idempotent) and return its
    /// data path. `created_for` is recorded on first creation only.
    pub fn ensure_volume(&self, name: &str, created_for: Option<&str>) -> Result<PathBuf> {
        check_name(name)?;
        let dir = self.volume_dir(name);
        let data = dir.join("data");
        fs::create_dir_all(&data).map_err(|e| io_err(&data, e))?;
        let meta_path = dir.join("volume.json");
        if !meta_path.exists() {
            let meta = VolumeMeta {
                name: name.to_string(),
                created_at: now_secs(),
                created_for: created_for.map(str::to_string),
            };
            let bytes = serde_json::to_vec_pretty(&meta).expect("volume meta serializes");
            fs::write(&meta_path, bytes).map_err(|e| io_err(&meta_path, e))?;
        }
        Ok(data)
    }

    /// Does `name` exist?
    pub fn has_volume(&self, name: &str) -> bool {
        valid_volume_name(name) && self.volume_dir(name).join("data").is_dir()
    }

    /// One volume with its measured size.
    pub fn volume_info(&self, name: &str) -> Result<VolumeInfo> {
        check_name(name)?;
        let dir = self.volume_dir(name);
        let data = dir.join("data");
        if !data.is_dir() {
            return Err(StoreError::VolumeMissing(name.to_string()));
        }
        let meta: VolumeMeta = fs::read(dir.join("volume.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(VolumeMeta {
                name: name.to_string(),
                created_at: 0,
                created_for: None,
            });
        Ok(VolumeInfo {
            name: name.to_string(),
            created_at: meta.created_at,
            created_for: meta.created_for,
            size_bytes: dir_size(&data),
            path: data,
        })
    }

    /// All volumes, sorted by name.
    pub fn list_volumes(&self) -> Result<Vec<VolumeInfo>> {
        let root = self.volumes_root();
        let mut out = Vec::new();
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(io_err(&root, e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&root, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(info) = self.volume_info(&name) {
                out.push(info);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Delete a volume and all of its data.
    pub fn remove_volume(&self, name: &str) -> Result<()> {
        check_name(name)?;
        let dir = self.volume_dir(name);
        if !dir.exists() {
            return Err(StoreError::VolumeMissing(name.to_string()));
        }
        // Containers may have created read-only subdirectories; restore
        // write permission on the way so removal always succeeds.
        make_tree_removable(&dir);
        fs::remove_dir_all(&dir).map_err(|e| io_err(&dir, e))
    }
}

fn make_tree_removable(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && !p.is_symlink() {
                make_tree_removable(&p);
            }
        }
    }
}

fn sanitize_component(s: &str) -> String {
    let cleaned: String = s
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    cleaned.trim_matches('-').to_string()
}

/// Short, human-friendly identifier for a declared volume path among its
/// siblings: the last path component (`/var/lib/postgresql/data` -> `data`),
/// extended with more components only when the short form would collide
/// with another declared path (mongo declares both `/data/db` and
/// `/data/configdb`; `/a/data` next to `/b/data` gives `a-data`/`b-data`).
pub fn stable_suffix(container_path: &str, all_paths: &[String]) -> String {
    let suffix_of = |path: &str, components: usize| -> String {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let start = parts.len().saturating_sub(components);
        sanitize_component(&parts[start..].join("-"))
    };
    let max = container_path
        .split('/')
        .filter(|p| !p.is_empty())
        .count()
        .max(1);
    for n in 1..=max {
        let suffix = suffix_of(container_path, n);
        let collision = all_paths
            .iter()
            .any(|other| other.as_str() != container_path && suffix_of(other, n) == suffix);
        if !collision {
            if suffix.is_empty() {
                return "data".to_string();
            }
            return suffix;
        }
    }
    // Everything collided (identical paths?) — use the full path.
    suffix_of(container_path, max)
}

/// Derive a stable, human-friendly volume name for a declared volume path
/// of an app: `<app>-<stable suffix>` (e.g. `redis-data`, `mongo-db`).
///
/// Stability is the whole point: the same app name + image volume always
/// maps to the same volume, so data survives stops and restarts without
/// the user managing anything.
pub fn stable_volume_name(app: &str, container_path: &str, all_paths: &[String]) -> String {
    let app = {
        let s = sanitize_component(app);
        if s.is_empty() {
            "app".to_string()
        } else {
            s
        }
    };
    let name = format!("{app}-{}", stable_suffix(container_path, all_paths));
    name.chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        (dir, store)
    }

    #[test]
    fn volume_lifecycle() {
        let (_d, store) = temp_store();
        assert!(store.list_volumes().unwrap().is_empty());
        assert!(!store.has_volume("redis-data"));

        let data = store
            .ensure_volume("redis-data", Some("redis:7-alpine"))
            .unwrap();
        assert!(data.is_dir());
        assert!(store.has_volume("redis-data"));
        // Idempotent: same path back, metadata untouched.
        assert_eq!(store.ensure_volume("redis-data", None).unwrap(), data);

        fs::write(data.join("dump.rdb"), b"0123456789").unwrap();
        fs::create_dir(data.join("sub")).unwrap();
        fs::write(data.join("sub/f"), b"abcde").unwrap();

        let info = store.volume_info("redis-data").unwrap();
        assert_eq!(info.size_bytes, 15);
        assert_eq!(info.created_for.as_deref(), Some("redis:7-alpine"));
        assert!(info.created_at > 0);
        assert_eq!(info.path, data);

        let all = store.list_volumes().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "redis-data");

        store.remove_volume("redis-data").unwrap();
        assert!(!store.has_volume("redis-data"));
        assert!(store.volume_info("redis-data").is_err());
        assert!(store.remove_volume("redis-data").is_err());
    }

    #[test]
    fn volume_names_are_validated() {
        assert!(valid_volume_name("redis-data"));
        assert!(valid_volume_name("a"));
        assert!(valid_volume_name("app.v2_data"));
        assert!(!valid_volume_name(""));
        assert!(!valid_volume_name("-leading"));
        assert!(!valid_volume_name(".hidden"));
        assert!(!valid_volume_name("UPPER"));
        assert!(!valid_volume_name("has space"));
        assert!(!valid_volume_name("path/name"));
        assert!(valid_volume_name("dots..name")); // odd but harmless: a single path component
        assert!(!valid_volume_name(&"x".repeat(65)));

        let (_d, store) = temp_store();
        assert!(store.ensure_volume("../escape", None).is_err());
        assert!(store.volume_info("a/b").is_err());
    }

    #[test]
    fn stable_names_are_short_and_collision_free() {
        // The common case: one declared volume, short suffix.
        assert_eq!(
            stable_volume_name("redis", "/data", &["/data".into()]),
            "redis-data"
        );
        assert_eq!(
            stable_volume_name(
                "postgres",
                "/var/lib/postgresql/data",
                &["/var/lib/postgresql/data".into()]
            ),
            "postgres-data"
        );
        // Deterministic: same inputs, same name.
        assert_eq!(
            stable_volume_name("redis", "/data", &["/data".into()]),
            stable_volume_name("redis", "/data", &["/data".into()])
        );
        // mongo declares /data/db and /data/configdb: no collision.
        let mongo: Vec<String> = vec!["/data/db".into(), "/data/configdb".into()];
        assert_eq!(stable_volume_name("mongo", "/data/db", &mongo), "mongo-db");
        assert_eq!(
            stable_volume_name("mongo", "/data/configdb", &mongo),
            "mongo-configdb"
        );
        // Same last component: the suffix widens until unambiguous.
        let twins: Vec<String> = vec!["/a/data".into(), "/b/data".into()];
        assert_eq!(stable_volume_name("app", "/a/data", &twins), "app-a-data");
        assert_eq!(stable_volume_name("app", "/b/data", &twins), "app-b-data");
        // Names come out valid whatever the input casing/characters.
        assert!(valid_volume_name(&stable_volume_name(
            "My App",
            "/Var/Data",
            &["/Var/Data".into()]
        )));
    }
}
