//! End-to-end `myc deploy` against the real binary, without any sshd: the
//! transport is injected via `$MYCEL_SSH_CMD` so the "remote" is this same
//! machine with a separate store (exactly how scripts/e2e.sh runs it too).

#![cfg(unix)]

use myc_manifest::{Entry, EntryKind, Manifest, RuntimeConfig, SCHEMA_VERSION};
use myc_store::Store;
use std::path::Path;
use std::process::Command;

const MYC: &str = env!("CARGO_BIN_EXE_myc");

fn entry(path: &str, hash: &str, size: u64) -> Entry {
    Entry {
        path: path.into(),
        kind: EntryKind::File,
        mode: 0o755,
        uid: 0,
        gid: 0,
        size,
        blake3: Some(hash.into()),
        target: None,
        device: None,
    }
}

fn manifest(name: &str, entries: Vec<Entry>) -> Manifest {
    Manifest {
        schema: SCHEMA_VERSION,
        name: name.into(),
        origin: None,
        os: "linux".into(),
        arch: "amd64".into(),
        created: "2026-01-01T00:00:00Z".into(),
        config: RuntimeConfig::default(),
        entries,
    }
}

/// `myc deploy` with the bash transport, returning (exit code, stdout+stderr).
fn deploy(local: &Path, remote: &Path, extra: &[&str]) -> (i32, String) {
    let out = Command::new(MYC)
        .env("MYCEL_STORE", local)
        .env(
            "MYCEL_SSH_CMD",
            format!("env MYCEL_STORE={} bash -c", remote.display()),
        )
        .args(["deploy"])
        .args(extra)
        .args(["--myc-path", MYC])
        .output()
        .expect("myc runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

#[test]
fn deploy_is_incremental_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let local_root = dir.path().join("local");
    let remote_root = dir.path().join("remote");

    // Local store: v1 (two files) and v2 (one shared, one new).
    {
        let local = Store::open(&local_root).unwrap();
        let (shared, _) = local.put_blob(&mut &b"shared library bytes"[..]).unwrap();
        let (only1, _) = local.put_blob(&mut &b"app version one"[..]).unwrap();
        let (only2, _) = local.put_blob(&mut &b"app version two!"[..]).unwrap();
        let v1 = manifest(
            "app:v1",
            vec![entry("/lib", &shared, 20), entry("/app", &only1, 15)],
        );
        let v2 = manifest(
            "app:v2",
            vec![entry("/lib", &shared, 20), entry("/app", &only2, 16)],
        );
        let id1 = local.put_manifest(&v1).unwrap();
        let id2 = local.put_manifest(&v2).unwrap();
        local.set_ref("app:v1", &id1).unwrap();
        local.set_ref("app:v2", &id2).unwrap();
    }

    // First deploy moves both blobs of v1.
    let (code, out) = deploy(&local_root, &remote_root, &["app:v1", "fake@remote"]);
    assert_eq!(code, 0, "first deploy failed:\n{out}");
    assert!(out.contains("2 files sent"), "unexpected report:\n{out}");

    // The remote store now resolves the ref and is complete.
    {
        let remote = Store::open(&remote_root).unwrap();
        let id = remote.resolve("app:v1").expect("ref registered remotely");
        let m = remote.get_manifest(&id).unwrap();
        assert!(remote.missing_blobs(&m).is_empty());
    }

    // Second deploy of the same ref: nothing crosses the wire.
    let (code, out) = deploy(&local_root, &remote_root, &["app:v1", "fake@remote"]);
    assert_eq!(code, 0, "re-deploy failed:\n{out}");
    assert!(out.contains("0 files sent"), "re-deploy sent data:\n{out}");
    assert!(
        out.contains("2 already present"),
        "unexpected report:\n{out}"
    );

    // v2 shares /lib with v1: only the one changed file is sent.
    let (code, out) = deploy(&local_root, &remote_root, &["app:v2", "fake@remote"]);
    assert_eq!(code, 0, "delta deploy failed:\n{out}");
    assert!(
        out.contains("1 file sent"),
        "expected a 1-file delta:\n{out}"
    );
    assert!(
        out.contains("1 already present"),
        "unexpected report:\n{out}"
    );
}

#[test]
fn bootstrap_uploads_the_binary_when_remote_has_none() {
    let dir = tempfile::tempdir().unwrap();
    let local_root = dir.path().join("local");
    let remote_root = dir.path().join("remote");
    let fake_home = dir.path().join("home");
    std::fs::create_dir_all(&fake_home).unwrap();

    {
        let local = Store::open(&local_root).unwrap();
        let (h, _) = local.put_blob(&mut &b"tiny"[..]).unwrap();
        let m = manifest("tiny:1", vec![entry("/t", &h, 4)]);
        let id = local.put_manifest(&m).unwrap();
        local.set_ref("tiny:1", &id).unwrap();
    }

    // A "remote" with no myc anywhere: empty PATH-visible dirs, fresh HOME.
    // No --myc-path, so `myc deploy` must probe, miss, and bootstrap.
    let out = Command::new(MYC)
        .env("MYCEL_STORE", &local_root)
        .env(
            "MYCEL_SSH_CMD",
            format!(
                "env MYCEL_STORE={} HOME={} PATH=/usr/bin:/bin bash -c",
                remote_root.display(),
                fake_home.display()
            ),
        )
        .args(["deploy", "tiny:1", "fake@remote"])
        .output()
        .expect("myc runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "bootstrap deploy failed:\n{text}"
    );
    assert!(text.contains("uploading this machine's binary"), "{text}");
    let installed = fake_home.join(".local/bin/myc");
    assert!(installed.is_file(), "binary not installed at {installed:?}");
    assert!(Store::open(&remote_root).unwrap().resolve("tiny:1").is_ok());

    // --no-bootstrap on a bare remote refuses instead of uploading.
    let out = Command::new(MYC)
        .env("MYCEL_STORE", &local_root)
        .env(
            "MYCEL_SSH_CMD",
            format!(
                "env MYCEL_STORE={} HOME={} PATH=/usr/bin:/bin bash -c",
                remote_root.display(),
                dir.path().join("home2").display()
            ),
        )
        .args(["deploy", "tiny:1", "fake@remote", "--no-bootstrap"])
        .output()
        .expect("myc runs");
    assert_ne!(out.status.code(), Some(0));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--no-bootstrap"), "{err}");
}
