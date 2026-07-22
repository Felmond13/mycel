//! Lazy rootfs integration tests: a real hub on localhost, a real FUSE
//! mount, and (where the environment provides a static busybox) a real
//! container run with on-demand blob faulting.

use myc_lazy::LazyRootfs;
use myc_manifest::{Entry, EntryKind, Manifest, RuntimeConfig, SCHEMA_VERSION};
use myc_store::Store;
use std::path::{Path, PathBuf};

fn file_entry(path: &str, mode: u32, size: u64, hash: &str) -> Entry {
    Entry {
        path: path.into(),
        kind: EntryKind::File,
        mode,
        uid: 0,
        gid: 0,
        size,
        blake3: Some(hash.into()),
        target: None,
        device: None,
    }
}

fn dir_entry(path: &str) -> Entry {
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

fn manifest(name: &str, entries: Vec<Entry>, cmd: Vec<String>) -> Manifest {
    Manifest {
        schema: SCHEMA_VERSION,
        name: name.into(),
        origin: None,
        os: "linux".into(),
        arch: "amd64".into(),
        created: "2026-01-01T00:00:00Z".into(),
        config: RuntimeConfig {
            cmd,
            ..Default::default()
        },
        entries,
    }
}

/// FUSE may be unavailable (no /dev/fuse, no fusermount3): skip, loudly.
fn fuse_available() -> bool {
    let ok = Path::new("/dev/fuse").exists();
    if !ok {
        eprintln!("SKIP: /dev/fuse not available in this environment");
    }
    ok
}

#[test]
fn lazy_mount_faults_in_and_caches() {
    if !fuse_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();

    // Hub store has both blobs; local store has only one.
    let hub_store = Store::open(dir.path().join("hub")).unwrap();
    let (h_local, s_local) = hub_store.put_blob(&mut &b"already here"[..]).unwrap();
    let (h_remote, s_remote) = hub_store.put_blob(&mut &b"streamed on demand"[..]).unwrap();
    let m = manifest(
        "lazy:t",
        vec![
            dir_entry("/data"),
            file_entry("/data/local.txt", 0o644, s_local, &h_local),
            file_entry("/data/remote.txt", 0o755, s_remote, &h_remote),
        ],
        vec![],
    );
    hub_store.put_manifest(&m).unwrap();
    drop(hub_store);
    let hub = myc_hub::spawn(dir.path().join("hub"), 0, None).unwrap();

    let local_root = dir.path().join("local");
    let local = Store::open(&local_root).unwrap();
    local.put_blob(&mut &b"already here"[..]).unwrap();

    let rootfs = dir.path().join("rootfs");
    let lazy = LazyRootfs::prepare(&local, &local_root, &m, &hub.url(), None, &rootfs).unwrap();
    assert_eq!(lazy.prepare_stats.local_files, 1);
    assert_eq!(lazy.prepare_stats.deferred_files, 1);
    assert_eq!(lazy.prepare_stats.deferred_bytes, s_remote);

    // The local file is a plain hardlink; the deferred one a symlink into
    // the mount (absolute, so resolve it manually host-side).
    let local_file = rootfs.join("data/local.txt");
    assert!(local_file.metadata().unwrap().is_file());
    let link = std::fs::read_link(rootfs.join("data/remote.txt")).unwrap();
    let in_mount = rootfs.join(link.to_str().unwrap().trim_start_matches('/'));

    // stat is answered from the manifest — no fetch yet.
    let meta = std::fs::metadata(&in_mount).unwrap();
    assert_eq!(meta.len(), s_remote);
    assert_eq!(lazy.run_stats().fetched_blobs, 0);

    // First read faults the blob in from the hub…
    assert_eq!(std::fs::read(&in_mount).unwrap(), b"streamed on demand");
    let stats = lazy.run_stats();
    assert_eq!(stats.fetched_blobs, 1);
    assert_eq!(stats.fetched_bytes, s_remote);
    assert_eq!(stats.errors, 0);
    // …and lands in the local store, cached for every future run.
    assert!(local.has_blob(&h_remote));

    // Second open is a local hit, not a second fetch.
    assert_eq!(std::fs::read(&in_mount).unwrap(), b"streamed on demand");
    let stats = lazy.run_stats();
    assert_eq!(stats.fetched_blobs, 1);
    assert!(stats.local_hits >= 1);

    lazy.cleanup().unwrap();
    assert!(!rootfs.exists());
}

#[test]
fn lazy_mount_write_is_rejected() {
    if !fuse_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let hub_store = Store::open(dir.path().join("hub")).unwrap();
    let (h, s) = hub_store.put_blob(&mut &b"immutable"[..]).unwrap();
    let m = manifest(
        "ro:t",
        vec![dir_entry("/d"), file_entry("/d/f", 0o644, s, &h)],
        vec![],
    );
    hub_store.put_manifest(&m).unwrap();
    drop(hub_store);
    let hub = myc_hub::spawn(dir.path().join("hub"), 0, None).unwrap();

    let local_root = dir.path().join("local");
    let local = Store::open(&local_root).unwrap();
    let rootfs = dir.path().join("rootfs");
    let lazy = LazyRootfs::prepare(&local, &local_root, &m, &hub.url(), None, &rootfs).unwrap();

    let link = std::fs::read_link(rootfs.join("d/f")).unwrap();
    let in_mount = rootfs.join(link.to_str().unwrap().trim_start_matches('/'));
    let err = std::fs::OpenOptions::new()
        .write(true)
        .open(&in_mount)
        .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EROFS));
    lazy.cleanup().unwrap();
}

/// Locate a busybox and its musl loader to serve as the container payload
/// (alpine's busybox is dynamically linked). Sourced from the user's real
/// Mycel store, which carries alpine in this development environment.
fn find_busybox() -> Option<(Vec<u8>, Vec<u8>)> {
    let store = Store::open(myc_store::Store::default_root()).ok()?;
    for (_, m) in store.list_manifests().ok()? {
        let bb = m
            .entries
            .iter()
            .find(|e| e.path == "/bin/busybox" && e.kind == EntryKind::File);
        let ld = m
            .entries
            .iter()
            .find(|e| e.path.starts_with("/lib/ld-musl") && e.kind == EntryKind::File);
        if let (Some(bb), Some(ld)) = (bb, ld) {
            let bb_path: PathBuf = store.blob(bb.blake3.as_deref()?).ok()?;
            let ld_path: PathBuf = store.blob(ld.blake3.as_deref()?).ok()?;
            return Some((std::fs::read(bb_path).ok()?, std::fs::read(ld_path).ok()?));
        }
    }
    None
}

/// The load-bearing spike: a FUSE mount created OUTSIDE the container
/// namespaces by a plain user must remain usable after `unshare` plus
/// recursive bind plus `pivot_root`, and must fault blobs in on demand
/// from inside the container.
#[test]
fn container_runs_from_lazy_rootfs() {
    if !fuse_available() {
        return;
    }
    if myc_run::probe_userns().is_err() {
        eprintln!("SKIP: user namespaces unavailable");
        return;
    }
    let Some((busybox, loader)) = find_busybox() else {
        eprintln!("SKIP: no busybox found on this machine");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let hub_store = Store::open(dir.path().join("hub")).unwrap();
    let (h_bb, s_bb) = hub_store.put_blob(&mut busybox.as_slice()).unwrap();
    let (h_ld, s_ld) = hub_store.put_blob(&mut loader.as_slice()).unwrap();
    let (h_data, s_data) = hub_store
        .put_blob(&mut &b"lazy-container-data"[..])
        .unwrap();
    let m = manifest(
        "spike:t",
        vec![
            dir_entry("/bin"),
            file_entry("/bin/busybox", 0o755, s_bb, &h_bb),
            dir_entry("/data"),
            file_entry("/data/msg", 0o644, s_data, &h_data),
            dir_entry("/lib"),
            file_entry("/lib/ld-musl-x86_64.so.1", 0o755, s_ld, &h_ld),
        ],
        vec!["/bin/busybox".into(), "cat".into(), "/data/msg".into()],
    );
    hub_store.put_manifest(&m).unwrap();
    drop(hub_store);
    let hub = myc_hub::spawn(dir.path().join("hub"), 0, None).unwrap();

    // Client store: completely empty. Everything must stream on demand —
    // including the binary being exec'd and the loader interpreting it.
    let local_root = dir.path().join("local");
    let local = Store::open(&local_root).unwrap();
    let rootfs = dir.path().join("rootfs");
    let lazy = LazyRootfs::prepare(&local, &local_root, &m, &hub.url(), None, &rootfs).unwrap();
    assert_eq!(lazy.prepare_stats.deferred_files, 3);

    let options = myc_run::RunOptions::default();
    let code = myc_run::exec_prepared(&m, &rootfs, &options).unwrap();
    assert_eq!(code, 0, "container should exit cleanly");

    // All three files were touched inside the container -> all fetched.
    let stats = lazy.run_stats();
    assert_eq!(stats.errors, 0);
    assert_eq!(stats.fetched_blobs, 3);
    assert_eq!(stats.fetched_bytes, s_bb + s_ld + s_data);
    // And they are now cached in the local store.
    assert!(local.has_blob(&h_bb));
    assert!(local.has_blob(&h_ld));
    assert!(local.has_blob(&h_data));

    lazy.cleanup().unwrap();
}
