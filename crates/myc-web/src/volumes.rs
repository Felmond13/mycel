//! App data endpoints: named volumes and per-container data mounts.
//!
//! The dashboard's promise is "your app's data just survives" — when a
//! container is started from the UI, every data directory the image
//! declares (OCI `Volumes`, stored on the manifest) is automatically
//! mapped to a *named volume* under `<store>/volumes/`, keyed by the app's
//! name. Start `redis`, stop it, start it again: same `redis-data` volume,
//! same data. Nobody has to know what a volume is.
//!
//! Power users can point an app at a folder of their computer instead
//! (`data_folder`), or opt out entirely (`keep_data: false`).

use crate::{bad_request, blocking, internal, not_found, open_store, ApiError, ApiResult, App};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use myc_store::volumes::{dir_size, stable_suffix, stable_volume_name};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Build the automatic data binds for a container start.
///
/// * `keep_data == false`: no binds — the app's data lives and dies with
///   the container (the pre-volumes behavior).
/// * `data_folder == None`: each declared volume maps to a stable named
///   volume derived from the app name (`redis` + `/data` -> `redis-data`).
/// * `data_folder == Some(dir)`: the user picked a folder of their
///   computer. One declared volume maps to the folder itself; several map
///   to subfolders (`dir/db`, `dir/configdb`, …).
pub(crate) fn auto_data_binds(
    store: &myc_store::Store,
    manifest: &myc_manifest::Manifest,
    app_name: &str,
    reference: &str,
    keep_data: bool,
    data_folder: Option<&str>,
) -> Result<Vec<String>, ApiError> {
    let declared = &manifest.config.volumes;
    if !keep_data {
        return Ok(Vec::new());
    }
    if declared.is_empty() {
        if data_folder.is_some() {
            return Err(bad_request(
                "this app does not declare a data directory, so there is nothing \
                 to store in that folder",
            ));
        }
        return Ok(Vec::new());
    }
    let mut binds = Vec::with_capacity(declared.len());
    match data_folder {
        Some(folder) => {
            let base = PathBuf::from(folder);
            if !base.is_absolute() {
                return Err(bad_request(format!(
                    "data folder must be an absolute path (got '{folder}')"
                )));
            }
            for path in declared {
                let host = if declared.len() == 1 {
                    base.clone()
                } else {
                    base.join(stable_suffix(path, declared))
                };
                std::fs::create_dir_all(&host).map_err(|e| {
                    bad_request(format!("cannot create data folder {}: {e}", host.display()))
                })?;
                binds.push(format!("{}:{path}", host.display()));
            }
        }
        None => {
            for path in declared {
                let name = stable_volume_name(app_name, path, declared);
                let data = store
                    .ensure_volume(&name, Some(reference))
                    .map_err(internal)?;
                binds.push(format!("{}:{path}", data.display()));
            }
        }
    }
    Ok(binds)
}

/// Resolve one stack `binds` entry (host path or named volume) into an
/// absolute `HOST:CONTAINER[:ro]` spec, creating named volumes on demand.
pub(crate) fn resolve_stack_mount(
    store: &myc_store::Store,
    spec: &str,
    base: &Path,
    created_for: &str,
) -> Result<String, ApiError> {
    let mount = myc_compose::parse_mount(spec, base).map_err(|e| bad_request(e.to_string()))?;
    let (host, container, ro) = match mount {
        myc_compose::Mount::Bind {
            host,
            container,
            read_only,
        } => (host, container, read_only),
        myc_compose::Mount::Volume {
            name,
            container,
            read_only,
        } => {
            let data = store
                .ensure_volume(&name, Some(created_for))
                .map_err(|e| bad_request(e.to_string()))?;
            (data, container, read_only)
        }
    };
    Ok(if ro {
        format!("{}:{container}:ro", host.display())
    } else {
        format!("{}:{container}", host.display())
    })
}

/// Automatic named volumes for a stack service: every volume the image
/// declares that is not already covered by an explicit `binds` entry gets
/// a stable `<stack>-<service>-<suffix>` volume, so stack data survives
/// `down`/`up` cycles with zero configuration.
pub(crate) fn stack_auto_binds(
    store: &myc_store::Store,
    manifest: &myc_manifest::Manifest,
    stack: &str,
    service: &str,
    explicit: &[String],
) -> Vec<String> {
    let declared = &manifest.config.volumes;
    let mut out = Vec::new();
    for path in declared {
        let covered = explicit
            .iter()
            .any(|b| b.split(':').nth(1) == Some(path.as_str()));
        if covered {
            continue;
        }
        let name = stable_volume_name(&format!("{stack}-{service}"), path, declared);
        if let Ok(data) = store.ensure_volume(&name, Some(&format!("stack {stack}/{service}"))) {
            out.push(format!("{}:{path}", data.display()));
        }
    }
    out
}

/// The `\\wsl.localhost\<distro>\...` twin of a WSL path — Windows users
/// can paste it into Explorer. `None` outside WSL.
pub(crate) fn windows_path_for(path: &Path) -> Option<String> {
    let distro = std::env::var("WSL_DISTRO_NAME").ok()?;
    Some(format!(
        "\\\\wsl.localhost\\{distro}{}",
        path.display().to_string().replace('/', "\\")
    ))
}

/// One parsed data mount of a container spec, as JSON for the UI.
fn mount_json(store_root: &Path, bind: &str) -> Option<Value> {
    // Specs were stored as absolute "HOST:CONTAINER[:ro]".
    let (host, container, ro) = match myc_compose::parse_bind(bind, Path::new("/")) {
        Ok(parts) => parts,
        Err(_) => return None,
    };
    let volumes_root = store_root.join("volumes");
    let volume_name = host
        .strip_prefix(&volumes_root)
        .ok()
        .and_then(|rest| rest.components().next())
        .map(|c| c.as_os_str().to_string_lossy().to_string());
    Some(json!({
        "container_path": container,
        "host_path": host.display().to_string(),
        "windows_path": windows_path_for(&host),
        "kind": if volume_name.is_some() { "volume" } else { "folder" },
        "volume": volume_name,
        "size_bytes": dir_size(&host),
        "read_only": ro,
    }))
}

/// `GET /api/containers/{id}/data`: the container's data mounts (named
/// volumes and bound folders), with sizes and host paths in both the
/// Linux and Windows spelling.
pub(crate) async fn container_data(
    State(app): State<App>,
    UrlPath(id): UrlPath<String>,
) -> ApiResult {
    blocking(move || {
        let spec = app
            .manager
            .spec_of(&id)
            .ok_or_else(|| not_found(format!("no container {id}")))?;
        let mounts: Vec<Value> = spec
            .binds
            .iter()
            .filter_map(|b| mount_json(&app.root, b))
            .collect();
        Ok(Json(json!({ "data": mounts })))
    })
    .await
}

/// Volume name -> names of *running* containers using it.
fn volumes_in_use(app: &App) -> std::collections::HashMap<String, Vec<String>> {
    let volumes_root = app.root.join("volumes");
    let mut used: std::collections::HashMap<String, Vec<String>> = Default::default();
    for c in app.manager.list() {
        if c["running"] != json!(true) {
            continue;
        }
        let (Some(id), Some(name)) = (c["id"].as_str(), c["name"].as_str()) else {
            continue;
        };
        let Some(spec) = app.manager.spec_of(id) else {
            continue;
        };
        for bind in &spec.binds {
            let Ok((host, _, _)) = myc_compose::parse_bind(bind, Path::new("/")) else {
                continue;
            };
            if let Some(vol) = host
                .strip_prefix(&volumes_root)
                .ok()
                .and_then(|rest| rest.components().next())
            {
                used.entry(vol.as_os_str().to_string_lossy().to_string())
                    .or_default()
                    .push(name.to_string());
            }
        }
    }
    used
}

/// `GET /api/volumes`: every named volume with size, host paths and the
/// running containers currently using it.
pub(crate) async fn list_volumes(State(app): State<App>) -> ApiResult {
    blocking(move || {
        let store = open_store(&app.root)?;
        let used = volumes_in_use(&app);
        let volumes: Vec<Value> = store
            .list_volumes()
            .map_err(internal)?
            .into_iter()
            .map(|v| {
                let used_by = used.get(&v.name).cloned().unwrap_or_default();
                json!({
                    "name": v.name,
                    "size_bytes": v.size_bytes,
                    "created_at": v.created_at,
                    "created_for": v.created_for,
                    "path": v.path.display().to_string(),
                    "windows_path": windows_path_for(&v.path),
                    "in_use": !used_by.is_empty(),
                    "used_by": used_by,
                })
            })
            .collect();
        Ok(Json(json!({ "volumes": volumes })))
    })
    .await
}

/// `DELETE /api/volumes/{name}`: delete a volume and its data. Refused
/// with `409` while a running container is using it.
pub(crate) async fn delete_volume(
    State(app): State<App>,
    UrlPath(name): UrlPath<String>,
) -> ApiResult {
    blocking(move || {
        let store = open_store(&app.root)?;
        if let Some(users) = volumes_in_use(&app).get(&name) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("'{name}' is in use by {} — stop it first", users.join(", ")),
            ));
        }
        store.remove_volume(&name).map_err(|e| match e {
            myc_store::StoreError::VolumeMissing(_) => not_found(e.to_string()),
            other => bad_request(other.to_string()),
        })?;
        Ok(Json(json!({ "removed": name })))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use myc_manifest::{Manifest, RuntimeConfig, SCHEMA_VERSION};

    fn manifest_with_volumes(volumes: &[&str]) -> Manifest {
        Manifest {
            schema: SCHEMA_VERSION,
            name: "test/app:1".into(),
            origin: None,
            os: "linux".into(),
            arch: "amd64".into(),
            created: "2026-01-01T00:00:00Z".into(),
            config: RuntimeConfig {
                volumes: volumes.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            entries: Vec::new(),
        }
    }

    fn temp_store() -> (tempfile::TempDir, myc_store::Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = myc_store::Store::open(dir.path().join("store")).unwrap();
        (dir, store)
    }

    #[test]
    fn keep_data_maps_declared_volumes_to_stable_named_volumes() {
        let (_d, store) = temp_store();
        let m = manifest_with_volumes(&["/data/configdb", "/data/db"]);
        let binds = auto_data_binds(&store, &m, "mongo", "mongo:7", true, None).unwrap();
        assert_eq!(binds.len(), 2);
        let configdb = store.volume_data_path("mongo-configdb").unwrap();
        let db = store.volume_data_path("mongo-db").unwrap();
        assert_eq!(binds[0], format!("{}:/data/configdb", configdb.display()));
        assert_eq!(binds[1], format!("{}:/data/db", db.display()));
        assert!(store.has_volume("mongo-db"));
        assert!(store.has_volume("mongo-configdb"));
        // Same app name again: the exact same volumes (that IS persistence).
        let again = auto_data_binds(&store, &m, "mongo", "mongo:7", true, None).unwrap();
        assert_eq!(binds, again);
    }

    #[test]
    fn keep_data_off_means_no_binds() {
        let (_d, store) = temp_store();
        let m = manifest_with_volumes(&["/data"]);
        let binds = auto_data_binds(&store, &m, "redis", "redis:7", false, None).unwrap();
        assert!(binds.is_empty());
        assert!(store.list_volumes().unwrap().is_empty());
    }

    #[test]
    fn data_folder_maps_single_volume_directly_and_multiple_to_subfolders() {
        let (dir, store) = temp_store();
        let folder = dir.path().join("my-redis-data");
        let m = manifest_with_volumes(&["/data"]);
        let binds = auto_data_binds(
            &store,
            &m,
            "redis",
            "redis:7",
            true,
            Some(folder.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(binds, vec![format!("{}:/data", folder.display())]);
        assert!(folder.is_dir());

        let m2 = manifest_with_volumes(&["/data/configdb", "/data/db"]);
        let folder2 = dir.path().join("my-mongo-data");
        let binds = auto_data_binds(
            &store,
            &m2,
            "mongo",
            "mongo:7",
            true,
            Some(folder2.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(
            binds,
            vec![
                format!("{}:/data/configdb", folder2.join("configdb").display()),
                format!("{}:/data/db", folder2.join("db").display()),
            ]
        );
        // No named volumes were created in folder mode.
        assert!(store.list_volumes().unwrap().is_empty());
        // Relative folders are refused.
        assert!(auto_data_binds(&store, &m, "redis", "redis:7", true, Some("rel/path")).is_err());
    }

    #[test]
    fn apps_without_declared_volumes_get_no_binds() {
        let (_d, store) = temp_store();
        let m = manifest_with_volumes(&[]);
        assert!(
            auto_data_binds(&store, &m, "alpine", "alpine:3.20", true, None)
                .unwrap()
                .is_empty()
        );
        // …and pointing a folder at them is a user error worth explaining.
        assert!(
            auto_data_binds(&store, &m, "alpine", "alpine:3.20", true, Some("/tmp/x")).is_err()
        );
    }

    #[test]
    fn stack_auto_binds_skip_paths_covered_explicitly() {
        let (_d, store) = temp_store();
        let m = manifest_with_volumes(&["/data"]);
        let explicit = vec!["/host/dir:/data".to_string()];
        assert!(stack_auto_binds(&store, &m, "shop", "cache", &explicit).is_empty());
        let auto = stack_auto_binds(&store, &m, "shop", "cache", &[]);
        assert_eq!(auto.len(), 1);
        assert!(store.has_volume("shop-cache-data"));
    }

    #[test]
    fn stack_mounts_resolve_names_and_paths() {
        let (dir, store) = temp_store();
        let base = dir.path();
        let spec = resolve_stack_mount(&store, "dbdata:/var/lib/db:ro", base, "stack s").unwrap();
        let vol = store.volume_data_path("dbdata").unwrap();
        assert_eq!(spec, format!("{}:/var/lib/db:ro", vol.display()));
        std::fs::create_dir_all(base.join("cfg")).unwrap();
        let spec = resolve_stack_mount(&store, "./cfg:/etc/app", base, "stack s").unwrap();
        assert_eq!(spec, format!("{}:/etc/app", base.join("./cfg").display()));
    }
}
