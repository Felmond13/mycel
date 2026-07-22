//! Hub protocol integration tests: a real axum server on an ephemeral
//! localhost port, a real reqwest client — no mocks.

use myc_hub::{transfer, HubClient};
use myc_manifest::{Entry, EntryKind, Manifest, RuntimeConfig, SCHEMA_VERSION};
use myc_store::Store;
use std::path::Path;

fn open_store(dir: &Path) -> Store {
    Store::open(dir).unwrap()
}

/// A manifest whose files are the given (path, content) pairs, with the
/// blobs inserted into `store`.
fn make_env(store: &Store, name: &str, files: &[(&str, &[u8])]) -> (String, Manifest) {
    let mut entries = vec![Entry {
        path: "/app".into(),
        kind: EntryKind::Dir,
        mode: 0o755,
        uid: 0,
        gid: 0,
        size: 0,
        blake3: None,
        target: None,
        device: None,
    }];
    for (path, content) in files {
        let (hash, size) = store.put_blob(&mut &content[..]).unwrap();
        entries.push(Entry {
            path: (*path).into(),
            kind: EntryKind::File,
            mode: 0o644,
            uid: 0,
            gid: 0,
            size,
            blake3: Some(hash),
            target: None,
            device: None,
        });
    }
    let manifest = Manifest {
        schema: SCHEMA_VERSION,
        name: name.into(),
        origin: None,
        os: "linux".into(),
        arch: "amd64".into(),
        created: "2026-01-01T00:00:00Z".into(),
        config: RuntimeConfig::default(),
        entries,
    };
    let id = store.put_manifest(&manifest).unwrap();
    store.set_ref(name, &id).unwrap();
    (id, manifest)
}

#[test]
fn ping_and_empty_listings() {
    let dir = tempfile::tempdir().unwrap();
    let hub = myc_hub::spawn(dir.path().join("hub"), 0, None).unwrap();
    let client = HubClient::new(&hub.url(), None).unwrap();

    let ping = client.ping().unwrap();
    assert_eq!(ping["ok"], true);
    assert_eq!(ping["service"], "mycel-hub");
    assert!(client.list_manifests().unwrap().is_empty());
    assert!(client.list_refs().unwrap().is_empty());
    assert!(client.get_ref("nope").unwrap().is_none());
}

#[test]
fn push_then_incremental_push() {
    let dir = tempfile::tempdir().unwrap();
    let hub = myc_hub::spawn(dir.path().join("hub"), 0, None).unwrap();
    let local = open_store(&dir.path().join("local"));
    let client = HubClient::new(&hub.url(), None).unwrap();

    let (_, m1) = make_env(
        &local,
        "app:v1",
        &[("/app/a", b"alpha".as_ref()), ("/app/b", b"beta".as_ref())],
    );
    let r1 = transfer::push(&local, &client, &m1, "app:v1", transfer::silent()).unwrap();
    assert_eq!(r1.blobs_total, 2);
    assert_eq!(r1.blobs_uploaded, 2);
    assert_eq!(r1.bytes_uploaded, 9);

    // Same content again: nothing crosses the wire.
    let r1b = transfer::push(&local, &client, &m1, "app:v1", transfer::silent()).unwrap();
    assert_eq!(r1b.blobs_uploaded, 0);
    assert_eq!(r1b.bytes_uploaded, 0);

    // One file changed: exactly one blob crosses the wire.
    let (_, m2) = make_env(
        &local,
        "app:v2",
        &[
            ("/app/a", b"alpha".as_ref()),
            ("/app/b", b"beta-two".as_ref()),
        ],
    );
    let r2 = transfer::push(&local, &client, &m2, "app:v2", transfer::silent()).unwrap();
    assert_eq!(r2.blobs_total, 2);
    assert_eq!(r2.blobs_uploaded, 1);
    assert_eq!(r2.bytes_uploaded, 8);

    // Hub state is complete and consistent.
    let hub_store = open_store(&dir.path().join("hub"));
    let hub_manifest = hub_store.get_manifest(&r2.manifest_id).unwrap();
    assert!(hub_store.missing_blobs(&hub_manifest).is_empty());
    assert_eq!(
        client.get_ref("app:v2").unwrap().as_deref(),
        Some(r2.manifest_id.as_str())
    );
}

#[test]
fn pull_only_missing_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let hub_store = open_store(&dir.path().join("hub"));
    let (_, manifest) = make_env(
        &hub_store,
        "web:v1",
        &[
            ("/app/x", b"xxxx".as_ref()),
            ("/app/y", b"yyyy".as_ref()),
            ("/app/z", b"zzzz".as_ref()),
        ],
    );
    drop(hub_store);
    let hub = myc_hub::spawn(dir.path().join("hub"), 0, None).unwrap();
    let client = HubClient::new(&hub.url(), None).unwrap();

    // The client already has one of the three blobs.
    let local = open_store(&dir.path().join("local"));
    local.put_blob(&mut &b"yyyy"[..]).unwrap();

    let report = transfer::pull(&local, &client, "web:v1", transfer::silent()).unwrap();
    assert_eq!(report.blobs_total, 3);
    assert_eq!(report.blobs_downloaded, 2);
    assert_eq!(report.bytes_downloaded, 8);
    assert!(local.missing_blobs(&manifest).is_empty());
    // Ref materialized locally, environment resolvable.
    assert_eq!(local.resolve("web:v1").unwrap(), report.manifest_id);

    // Pulling again is a no-op.
    let again = transfer::pull(&local, &client, "web:v1", transfer::silent()).unwrap();
    assert_eq!(again.blobs_downloaded, 0);
}

#[test]
fn server_rejects_corruption_and_bad_ids() {
    let dir = tempfile::tempdir().unwrap();
    let hub = myc_hub::spawn(dir.path().join("hub"), 0, None).unwrap();
    let url = hub.url();
    let http = reqwest::blocking::Client::new();

    // Blob whose content does not match the claimed hash.
    let bogus = "0".repeat(64);
    let resp = http
        .put(format!("{url}/api/v1/blobs/{bogus}"))
        .body("not the zero hash")
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().unwrap().contains("hash mismatch"));

    // Malformed hash (path traversal shaped) is rejected before touching disk.
    let resp = http
        .put(format!("{url}/api/v1/blobs/deadbeef"))
        .body("x")
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Manifest under a wrong id.
    let store = Store::open(dir.path().join("scratch")).unwrap();
    let (_, manifest) = make_env(&store, "m:1", &[("/app/f", b"data".as_ref())]);
    let wrong_id = format!("myc1-{}", "1".repeat(64));
    let resp = http
        .put(format!("{url}/api/v1/manifests/{wrong_id}"))
        .header("content-type", "application/json")
        .body(manifest.to_canonical_json().unwrap())
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().unwrap().contains("mismatch"));

    // Ref pointing at a manifest the hub does not have.
    let resp = http
        .put(format!("{url}/api/v1/refs/ghost"))
        .json(&serde_json::json!({"manifest_id": format!("myc1-{}", "2".repeat(64))}))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[test]
fn token_protects_writes_not_reads() {
    let dir = tempfile::tempdir().unwrap();
    let hub = myc_hub::spawn(dir.path().join("hub"), 0, Some("s3cret".into())).unwrap();
    let local = open_store(&dir.path().join("local"));
    let (_, manifest) = make_env(&local, "sec:v1", &[("/app/f", b"private".as_ref())]);

    // Unauthenticated write: rejected.
    let anon = HubClient::new(&hub.url(), None).unwrap();
    let err = transfer::push(&local, &anon, &manifest, "sec:v1", transfer::silent()).unwrap_err();
    assert!(err.to_string().contains("401") || err.to_string().contains("bearer token"));

    // Wrong token: rejected.
    let wrong = HubClient::new(&hub.url(), Some("nope".into())).unwrap();
    assert!(transfer::push(&local, &wrong, &manifest, "sec:v1", transfer::silent()).is_err());

    // Correct token: accepted; reads work anonymously.
    let auth = HubClient::new(&hub.url(), Some("s3cret".into())).unwrap();
    transfer::push(&local, &auth, &manifest, "sec:v1", transfer::silent()).unwrap();
    assert!(anon.get_ref("sec:v1").unwrap().is_some());
    assert_eq!(anon.list_manifests().unwrap().len(), 1);
}

#[test]
fn missing_negotiation() {
    let dir = tempfile::tempdir().unwrap();
    let hub_store = open_store(&dir.path().join("hub"));
    let (have, _) = hub_store.put_blob(&mut &b"present"[..]).unwrap();
    drop(hub_store);
    let hub = myc_hub::spawn(dir.path().join("hub"), 0, None).unwrap();
    let client = HubClient::new(&hub.url(), None).unwrap();

    let absent = blake3::hash(b"absent").to_hex().to_string();
    let missing = client.missing(&[have.clone(), absent.clone()]).unwrap();
    assert_eq!(missing, vec![absent]);
}

#[test]
fn pull_manifest_only_fetches_no_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let hub_store = open_store(&dir.path().join("hub"));
    let (id, _) = make_env(&hub_store, "lazy:v1", &[("/app/big", b"payload".as_ref())]);
    drop(hub_store);
    let hub = myc_hub::spawn(dir.path().join("hub"), 0, None).unwrap();
    let client = HubClient::new(&hub.url(), None).unwrap();

    let local = open_store(&dir.path().join("local"));
    let (got_id, manifest) = transfer::pull_manifest_only(&local, &client, "lazy:v1").unwrap();
    assert_eq!(got_id, id);
    // Manifest present, blobs deliberately not.
    assert_eq!(local.resolve("lazy:v1").unwrap(), id);
    assert_eq!(local.missing_blobs(&manifest).len(), 1);
}
