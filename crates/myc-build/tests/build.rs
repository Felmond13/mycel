//! End-to-end tests of the build pipeline against real temp stores.

use myc_build::{build, spec::BuildSpec, BuildRequest, NoProgress};
use myc_manifest::{Entry, EntryKind, Manifest, RuntimeConfig, SCHEMA_VERSION};
use myc_store::Store;
use std::fs;
use std::path::Path;

fn temp() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("store")).unwrap();
    (dir, store)
}

fn request<'a>(
    spec: &'a BuildSpec,
    ctx: &'a Path,
    base: Option<&'a Manifest>,
    work: &'a Path,
) -> BuildRequest<'a> {
    BuildRequest {
        spec,
        context_dir: ctx,
        base,
        name_override: None,
        timestamp: Some("2026-01-01T00:00:00Z"),
        origin: Some("build://test".to_string()),
        work_dir: work,
    }
}

fn base_manifest(store: &Store) -> Manifest {
    let (hash, size) = store.put_blob(&mut &b"base file"[..]).unwrap();
    let dir_entry = |path: &str| Entry {
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
    let m = Manifest {
        schema: SCHEMA_VERSION,
        name: "base:1".into(),
        origin: None,
        os: "linux".into(),
        arch: "amd64".into(),
        created: "2026-01-01T00:00:00Z".into(),
        config: RuntimeConfig {
            env: vec!["PATH=/bin".into(), "LANG=C".into()],
            cmd: vec!["/bin/sh".into()],
            ..Default::default()
        },
        entries: vec![
            dir_entry("/app"),
            Entry {
                path: "/app/base".into(),
                kind: EntryKind::File,
                mode: 0o644,
                uid: 42,
                gid: 42,
                size,
                blake3: Some(hash),
                target: None,
                device: None,
            },
            dir_entry("/etc"),
        ],
    };
    store.put_manifest(&m).unwrap();
    m
}

#[test]
fn scratch_build_with_adds() {
    let (dir, store) = temp();
    let ctx = dir.path().join("ctx");
    fs::create_dir_all(ctx.join("dist")).unwrap();
    fs::write(ctx.join("dist/app"), b"#!/bin/sh\necho app\n").unwrap();

    let spec = BuildSpec::from_toml_str(
        r#"
[build]
name = "scratch-app:1"
[[build.add]]
source = "dist/app"
dest = "/usr/local/bin/app"
mode = 0o755
[config]
entrypoint = ["/usr/local/bin/app"]
"#,
    )
    .unwrap();

    let report = build(
        &store,
        &request(&spec, &ctx, None, dir.path()),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(report.name, "scratch-app:1");
    assert_eq!(report.file_count, 1);
    assert_eq!(report.blobs_added, 1);

    let m = store.get_manifest(&report.manifest_id).unwrap();
    // Parent dirs auto-created, sorted before children.
    let paths: Vec<&str> = m.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["/usr", "/usr/local", "/usr/local/bin", "/usr/local/bin/app"]
    );
    assert_eq!(m.entries[3].mode, 0o755);
    assert_eq!(m.config.entrypoint, vec!["/usr/local/bin/app"]);
    assert_eq!(store.resolve("scratch-app:1").unwrap(), report.manifest_id);
    assert!(store.missing_blobs(&m).is_empty());
}

#[test]
fn add_merges_over_base_and_shadows_dirs() {
    let (dir, store) = temp();
    let base = base_manifest(&store);
    let ctx = dir.path().join("ctx");
    fs::create_dir_all(&ctx).unwrap();
    fs::write(ctx.join("new"), b"new content").unwrap();
    fs::write(ctx.join("app-file"), b"app is now a file").unwrap();

    // Add a file over /app (a dir in the base): subtree must vanish.
    let spec = BuildSpec::from_toml_str(
        r#"
[build]
base = "base:1"
name = "merged:1"
[[build.add]]
source = "new"
dest = "/etc/new"
[[build.add]]
source = "app-file"
dest = "/app"
"#,
    )
    .unwrap();
    let report = build(
        &store,
        &request(&spec, &ctx, Some(&base), dir.path()),
        &mut NoProgress,
    )
    .unwrap();
    let m = store.get_manifest(&report.manifest_id).unwrap();
    let get = |p: &str| m.entries.iter().find(|e| e.path == p);
    assert_eq!(get("/app").unwrap().kind, EntryKind::File);
    assert!(get("/app/base").is_none(), "shadowed subtree must be gone");
    assert!(get("/etc/new").is_some());
    // Base config inherited untouched.
    assert_eq!(m.config.cmd, vec!["/bin/sh"]);
    assert_eq!(m.config.env, vec!["PATH=/bin", "LANG=C"]);
}

#[test]
fn config_env_merges_over_base() {
    let (dir, store) = temp();
    let base = base_manifest(&store);
    let spec = BuildSpec::from_toml_str(
        r#"
[build]
base = "base:1"
name = "cfg:1"
[config]
env = ["LANG=fr_FR.UTF-8", "PORT=8080"]
cmd = ["/bin/app"]
"#,
    )
    .unwrap();
    let ctx = dir.path().to_path_buf();
    let report = build(
        &store,
        &request(&spec, &ctx, Some(&base), dir.path()),
        &mut NoProgress,
    )
    .unwrap();
    let m = store.get_manifest(&report.manifest_id).unwrap();
    assert_eq!(
        m.config.env,
        vec!["PATH=/bin", "LANG=fr_FR.UTF-8", "PORT=8080"]
    );
    assert_eq!(m.config.cmd, vec!["/bin/app"]);
}

#[test]
fn deterministic_with_fixed_timestamp() {
    let (dir, store) = temp();
    let ctx = dir.path().join("ctx");
    fs::create_dir_all(ctx.join("d")).unwrap();
    fs::write(ctx.join("d/a"), b"aaa").unwrap();
    fs::write(ctx.join("d/b"), b"bbb").unwrap();

    let spec = BuildSpec::from_toml_str(
        r#"
[build]
name = "det:1"
[[build.add]]
source = "d"
dest = "/data"
"#,
    )
    .unwrap();
    let r1 = build(
        &store,
        &request(&spec, &ctx, None, dir.path()),
        &mut NoProgress,
    )
    .unwrap();
    let r2 = build(
        &store,
        &request(&spec, &ctx, None, dir.path()),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(r1.manifest_id, r2.manifest_id);
    // Second build stored zero new bytes: everything deduped.
    assert_eq!(r2.bytes_added, 0);
    assert_eq!(r2.blobs_added, 0);
}

#[test]
fn incremental_rebuild_adds_only_changed_file() {
    let (dir, store) = temp();
    let ctx = dir.path().join("ctx");
    fs::create_dir_all(ctx.join("d")).unwrap();
    fs::write(ctx.join("d/stable"), b"never changes").unwrap();
    fs::write(ctx.join("d/hot"), b"v1").unwrap();

    let spec = BuildSpec::from_toml_str(
        "[build]\nname = \"inc:1\"\n[[build.add]]\nsource = \"d\"\ndest = \"/data\"\n",
    )
    .unwrap();
    let r1 = build(
        &store,
        &request(&spec, &ctx, None, dir.path()),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(r1.blobs_added, 2);

    fs::write(ctx.join("d/hot"), b"v2 - changed").unwrap();
    let r2 = build(
        &store,
        &request(&spec, &ctx, None, dir.path()),
        &mut NoProgress,
    )
    .unwrap();
    assert_ne!(r1.manifest_id, r2.manifest_id);
    assert_eq!(r2.blobs_added, 1, "only the changed file hits the store");
    assert_eq!(r2.bytes_added, b"v2 - changed".len() as u64);
}

#[test]
fn missing_source_fails() {
    let (dir, store) = temp();
    let spec = BuildSpec::from_toml_str(
        "[build]\nname = \"x:1\"\n[[build.add]]\nsource = \"nope\"\ndest = \"/f\"\n",
    )
    .unwrap();
    let ctx = dir.path().to_path_buf();
    let err = build(
        &store,
        &request(&spec, &ctx, None, dir.path()),
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");
}

#[test]
fn missing_name_fails() {
    let (dir, store) = temp();
    let spec = BuildSpec::from_toml_str("[build]\n").unwrap();
    let ctx = dir.path().to_path_buf();
    let err = build(
        &store,
        &request(&spec, &ctx, None, dir.path()),
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(err.to_string().contains("no name"), "{err}");
}

/// Real rootless-namespace build step. Requires working unprivileged user
/// namespaces and a base with /bin/sh, so it builds its own tiny base from
/// the host's static busybox when available; exercised for real by
/// scripts/e2e.sh against alpine.
#[test]
#[ignore = "needs unprivileged user namespaces; run explicitly or via scripts/e2e.sh"]
fn run_step_records_created_files() {
    let (dir, store) = temp();

    // Tiny base: busybox + /bin/sh symlink. Skip when the host has no
    // static busybox — scripts/e2e.sh covers this path against real alpine.
    let Some(busybox) = ["/bin/busybox", "/usr/bin/busybox"]
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
    else {
        eprintln!("skipping: no busybox binary on this host");
        return;
    };
    let mut f = fs::File::open(busybox).unwrap();
    let (hash, size) = store.put_blob_with_mode(&mut f, 0o755).unwrap();
    let entry =
        |path: &str, kind: EntryKind, blake3: Option<String>, target: Option<String>| Entry {
            path: path.into(),
            kind,
            mode: 0o755,
            uid: 0,
            gid: 0,
            size: if kind == EntryKind::File { size } else { 0 },
            blake3,
            target,
            device: None,
        };
    let base = Manifest {
        schema: SCHEMA_VERSION,
        name: "bb:1".into(),
        origin: None,
        os: "linux".into(),
        arch: "amd64".into(),
        created: "2026-01-01T00:00:00Z".into(),
        config: RuntimeConfig::default(),
        entries: vec![
            entry("/bin", EntryKind::Dir, None, None),
            entry("/bin/busybox", EntryKind::File, Some(hash), None),
            entry("/bin/sh", EntryKind::Symlink, None, Some("busybox".into())),
        ],
    };
    store.put_manifest(&base).unwrap();

    let spec = BuildSpec::from_toml_str(
        r#"
[build]
base = "bb:1"
name = "stamped:1"
[[build.run]]
command = ["/bin/sh", "-c", "echo built-by-mycel > /stamp && rm /bin/busybox"]
"#,
    )
    .unwrap();
    let ctx = dir.path().to_path_buf();
    let report = build(
        &store,
        &request(&spec, &ctx, Some(&base), dir.path()),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(report.steps.len(), 1);
    assert_eq!(report.steps[0].added, 1);
    assert_eq!(report.steps[0].removed, 1);

    let m = store.get_manifest(&report.manifest_id).unwrap();
    let stamp = m.entries.iter().find(|e| e.path == "/stamp").unwrap();
    let blob = store.blob(stamp.blake3.as_deref().unwrap()).unwrap();
    assert_eq!(fs::read(blob).unwrap(), b"built-by-mycel\n");
    assert!(!m.entries.iter().any(|e| e.path == "/bin/busybox"));
}
