//! Chunk-negotiation integration tests (hub protocol v2): a real chunked
//! axum server on an ephemeral port, real chunked stores, real reqwest —
//! and the number that justifies it all: how many bytes a chunk-mode
//! transfer moves versus the v1 whole-blob path for the same scenario.
//!
//! Scenario (the lab version of alpine 3.20 → 3.21): store A ingests a
//! base environment E1 and a derived E2 (small edits inside big files,
//! one new small file). A chunked hub pre-loaded with E1 receives E2 from
//! A (push delta). Store B, which already ingested E1 locally, pulls E2
//! from that hub (pull delta). Controls run the identical scenario through
//! the v1 whole-blob path — same content, same starting states — and the
//! byte counts of the two reports are compared.

use myc_hub::{transfer, HubClient};
use myc_manifest::{Entry, EntryKind, Manifest, RuntimeConfig, SCHEMA_VERSION};
use myc_store::Store;

// ---------------------------------------------------------------------------
// Deterministic content
// ---------------------------------------------------------------------------

/// SplitMix64 byte generator — same spirit as the myc-store test helper;
/// no ambient randomness, so every run produces identical blobs.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut z = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        self.0 = z;
        z ^ (z >> 31)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next_u64() >> 32) as u8).collect()
    }
}

/// ~224 KiB with realistic structure: random head, a repeated middle, and
/// a tail copied from the head with a small edit — big enough to be chunked
/// (≥ 64 KiB), similar enough between seeds/edits for the CDC to shine.
fn structured_blob(seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let head = rng.bytes(96 * 1024);
    let mut data = head.clone();
    let repeated = rng.bytes(4 * 1024);
    for _ in 0..16 {
        data.extend_from_slice(&repeated);
    }
    let mut tail = head[..64 * 1024].to_vec();
    tail[..64].copy_from_slice(&rng.bytes(64));
    data.extend_from_slice(&tail);
    data
}

/// The two environments: E2 is E1 after a "release" — two big files with
/// small edits and an append, one unchanged small file, one new small file.
fn env_v1() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("/usr/bin/bigtool", structured_blob(1)),
        ("/usr/lib/libbig.so", structured_blob(2)),
        ("/etc/config", Rng(3).bytes(1024)),
    ]
}

fn env_v2() -> Vec<(&'static str, Vec<u8>)> {
    let mut bigtool = structured_blob(1);
    let mid = bigtool.len() / 2;
    for byte in &mut bigtool[mid..mid + 64] {
        *byte ^= 0x5a;
    }
    bigtool.extend_from_slice(&vec![9u8; 8192]);
    let mut lib = structured_blob(2);
    for byte in &mut lib[..64] {
        *byte ^= 0x5a;
    }
    vec![
        ("/usr/bin/bigtool", bigtool),
        ("/usr/lib/libbig.so", lib),
        ("/etc/config", Rng(3).bytes(1024)), // unchanged → blob-level dedup
        ("/etc/newfile", Rng(4).bytes(2048)), // new, below the chunking size
    ]
}

/// Insert the files into `store` and build the manifest (same shape as the
/// hub.rs helper, for content passed in rather than inline literals).
fn make_env(store: &Store, name: &str, files: &[(&str, Vec<u8>)]) -> (String, Manifest) {
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

fn pct(saved: u64, total: u64) -> f64 {
    (saved as f64 / total as f64) * 100.0
}

// ---------------------------------------------------------------------------
// The headline test
// ---------------------------------------------------------------------------

#[test]
fn chunk_transfer_moves_a_fraction_of_the_bytes_and_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let files1 = env_v1();
    let files2 = env_v2();

    // Store A (chunked) has both versions.
    let a = Store::open_chunked(dir.path().join("a")).unwrap();
    let (id1, m1) = make_env(&a, "app:3.20", &files1);
    let (id2, m2) = make_env(&a, "app:3.21", &files2);
    let _ = id1;

    // --- PUSH: chunked hub pre-loaded with E1, then E2 (the delta) ---
    let hub = myc_hub::spawn_chunked(dir.path().join("hub"), 0, None).unwrap();
    let client = HubClient::new(&hub.url(), None).unwrap();
    let base = transfer::push(&a, &client, &m1, "app:3.20", transfer::silent()).unwrap();
    assert!(base.chunked, "both ends chunked → chunk protocol");
    let push2 = transfer::push(&a, &client, &m2, "app:3.21", transfer::silent()).unwrap();
    assert!(push2.chunked);
    assert!(push2.chunks_uploaded > 0, "the delta is real chunks");
    assert_eq!(push2.blobs_uploaded, 3, "three new blobs (config deduped)");

    // --- PUSH control: same scenario against a v1 hub (automatic fallback) ---
    let hub_v1 = myc_hub::spawn(dir.path().join("hub-v1"), 0, None).unwrap();
    let client_v1 = HubClient::new(&hub_v1.url(), None).unwrap();
    transfer::push(&a, &client_v1, &m1, "app:3.20", transfer::silent()).unwrap();
    let push2_v1 = transfer::push(&a, &client_v1, &m2, "app:3.21", transfer::silent()).unwrap();
    assert!(!push2_v1.chunked, "v1 hub → blob fallback");
    assert_eq!(push2_v1.chunks_uploaded, 0);
    assert_eq!(push2_v1.blobs_uploaded, 3);

    let push_saved = push2_v1.bytes_uploaded - push2.bytes_uploaded;
    println!(
        "push E2 after E1: v1 blobs {} B vs v2 chunks {} B ({} chunks) — {:.1}% saved",
        push2_v1.bytes_uploaded,
        push2.bytes_uploaded,
        push2.chunks_uploaded,
        pct(push_saved, push2_v1.bytes_uploaded),
    );
    assert!(
        push2.bytes_uploaded * 2 < push2_v1.bytes_uploaded,
        "chunk push should move less than half the bytes: {} vs {}",
        push2.bytes_uploaded,
        push2_v1.bytes_uploaded
    );

    // --- PULL: chunked store B already has E1 locally, pulls E2 ---
    let b = Store::open_chunked(dir.path().join("b")).unwrap();
    make_env(&b, "app:3.20", &files1); // same bytes → same hashes, chunks in B's packs
    let client_b = HubClient::new(&hub.url(), None).unwrap();
    let pull2 = transfer::pull(&b, &client_b, "app:3.21", transfer::silent()).unwrap();
    assert!(pull2.chunked);
    assert!(pull2.chunks_downloaded > 0);
    assert_eq!(pull2.blobs_downloaded, 3);

    // --- PULL control: a v1 store in the same state pulls from the SAME
    // chunk-capable hub (the v1-client fallback path) ---
    let b_v1 = Store::open(dir.path().join("b-v1")).unwrap();
    make_env(&b_v1, "app:3.20", &files1);
    let pull2_v1 = transfer::pull(&b_v1, &client_b, "app:3.21", transfer::silent()).unwrap();
    assert!(!pull2_v1.chunked);
    assert_eq!(pull2_v1.chunks_downloaded, 0);

    let pull_saved = pull2_v1.bytes_downloaded - pull2.bytes_downloaded;
    println!(
        "pull E2 with E1 local: v1 blobs {} B vs v2 chunks {} B ({} chunks) — {:.1}% saved",
        pull2_v1.bytes_downloaded,
        pull2.bytes_downloaded,
        pull2.chunks_downloaded,
        pct(pull_saved, pull2_v1.bytes_downloaded),
    );
    assert!(
        pull2.bytes_downloaded * 2 < pull2_v1.bytes_downloaded,
        "chunk pull should move less than half the bytes: {} vs {}",
        pull2.bytes_downloaded,
        pull2_v1.bytes_downloaded
    );

    // --- Round-trip integrity on B: complete, resolvable, reassemblable ---
    assert!(b.missing_blobs(&m2).is_empty(), "offline-ready after pull");
    assert_eq!(b.resolve("app:3.21").unwrap(), id2);
    for (path, content) in &files2 {
        let hash = blake3::hash(content).to_hex().to_string();
        b.verify_blob(&hash).unwrap();
        if b.blob_chunks(&hash).unwrap().is_some() {
            assert_eq!(
                &b.reassemble_blob(&hash).unwrap(),
                content,
                "reassembled bytes differ for {path}"
            );
        }
    }

    // The hub itself holds complete v1 blobs (v1 pulls keep working) and a
    // populated pack layer (future chunk pulls benefit).
    let hub_store = Store::open_chunked(dir.path().join("hub")).unwrap();
    assert!(hub_store.missing_blobs(&m2).is_empty());
    assert!(hub_store.chunk_stats().unwrap().packs > 0);

    // Re-pull and re-push move nothing.
    let pull_again = transfer::pull(&b, &client_b, "app:3.21", transfer::silent()).unwrap();
    assert_eq!(pull_again.bytes_downloaded, 0);
    assert_eq!(pull_again.chunks_downloaded, 0);
    let push_again = transfer::push(&a, &client, &m2, "app:3.21", transfer::silent()).unwrap();
    assert_eq!(push_again.bytes_uploaded, 0);
    assert_eq!(push_again.chunks_uploaded, 0);
    assert_eq!(push_again.blobs_uploaded, 0);

    // Server-side cost, for the record (debug build, one blob set):
    println!(
        "elapsed push E2: chunk {:.2?} vs blob {:.2?} | pull E2: chunk {:.2?} vs blob {:.2?}",
        push2.elapsed, push2_v1.elapsed, pull2.elapsed, pull2_v1.elapsed
    );
}

// ---------------------------------------------------------------------------
// Endpoint behavior and integrity guarantees
// ---------------------------------------------------------------------------

#[test]
fn chunk_endpoints_verify_integrity() {
    let dir = tempfile::tempdir().unwrap();
    // A chunked hub store holding one structured blob.
    let hub_root = dir.path().join("hub");
    let seed = Store::open_chunked(&hub_root).unwrap();
    let data = structured_blob(7);
    let (hash, _) = seed.put_blob(&mut &data[..]).unwrap();
    let ids = seed.blob_chunks(&hash).unwrap().unwrap();
    drop(seed);
    let hub = myc_hub::spawn_chunked(hub_root.clone(), 0, None).unwrap();
    let url = hub.url();
    let http = reqwest::blocking::Client::new();

    // GET one chunk: 200, raw bytes hashing to the id.
    let some_chunk = hex::encode(ids[0]);
    let resp = http
        .get(format!("{url}/api/v1/chunks/{some_chunk}"))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.bytes().unwrap();
    assert_eq!(blake3::hash(&body).to_hex().to_string(), some_chunk);

    // Unknown chunk: 404. Malformed id: 400 before touching disk.
    let absent = blake3::hash(b"nowhere").to_hex().to_string();
    assert_eq!(
        http.get(format!("{url}/api/v1/chunks/{absent}"))
            .send()
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        http.get(format!("{url}/api/v1/chunks/short"))
            .send()
            .unwrap()
            .status(),
        400
    );

    // POST a chunk whose body does not match its id: rejected, like blobs.
    let resp = http
        .post(format!("{url}/api/v1/chunks/{some_chunk}"))
        .body("wrong bytes")
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().unwrap().contains("mismatch"));

    // Negotiation: present chunk not missing, absent one missing, hint
    // points at a real pack.
    let req = serde_json::json!({
        "blob_hash": blake3::hash(b"some other blob").to_hex().to_string(),
        "chunk_ids": [some_chunk.clone(), absent.clone()],
    });
    let resp: serde_json::Value = http
        .post(format!("{url}/api/v1/chunks/missing"))
        .json(&req)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(resp["missing"], serde_json::json!([absent.clone()]));
    let hints = resp["pack_hints"].as_array().unwrap();
    assert_eq!(hints.len(), 1, "one pack holds the present chunk");
    assert!(hub_root
        .join("packs")
        .join(format!("pack-{}.zst", hints[0].as_str().unwrap()))
        .exists());

    // Blob recipe: the ordered sidecar; unknown blob: 404.
    let resp: serde_json::Value = http
        .get(format!("{url}/api/v1/blobs/{hash}/chunks"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let got: Vec<String> = resp["chunk_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(got, ids.iter().map(hex::encode).collect::<Vec<_>>());
    assert_eq!(
        http.get(format!("{url}/api/v1/blobs/{absent}/chunks"))
            .send()
            .unwrap()
            .status(),
        404
    );

    // Assembly with a chunk the hub does not have: 400, blob not stored.
    let resp = http
        .post(format!("{url}/api/v1/blobs/{absent}/chunks"))
        .json(&serde_json::json!({ "chunk_ids": [absent.clone()] }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(resp.text().unwrap().contains("missing 1 chunk"));
}

/// The interrupted-push resume path: staged chunks count as present in the
/// negotiation (no re-upload), and an assembly can build a blob purely from
/// the hub's own packs when every chunk is already there.
#[test]
fn assembly_mixes_staged_chunks_and_hub_packs() {
    let dir = tempfile::tempdir().unwrap();
    let hub_root = dir.path().join("hub");
    let seed = Store::open_chunked(&hub_root).unwrap();
    let data = structured_blob(9);
    let (_, _) = seed.put_blob(&mut &data[..]).unwrap();
    let hash_of = |bytes: &[u8]| blake3::hash(bytes).to_hex().to_string();
    let (sidecar_hash, _) = seed.put_blob(&mut &structured_blob(10)[..]).unwrap();
    let ids = seed.blob_chunks(&sidecar_hash).unwrap().unwrap();
    drop(seed);

    let hub = myc_hub::spawn_chunked(hub_root.clone(), 0, None).unwrap();
    let url = hub.url();
    let http = reqwest::blocking::Client::new();

    // Three real chunk bytes from the hub's packs, recombined into a brand
    // new blob the hub has never seen.
    let locator = myc_hub::chunkio::ChunkLocator::scan(&hub_root);
    let picked: Vec<Vec<u8>> = ids[..3]
        .iter()
        .map(|id| myc_hub::chunkio::read_chunk(&hub_root, &locator, id).unwrap())
        .collect();
    let novel: Vec<u8> = picked.concat();
    let novel_hash = hash_of(&novel);
    let picked_ids: Vec<String> = ids[..3].iter().map(hex::encode).collect();

    // All chunks already in the hub's packs: nothing missing, assembly
    // alone creates the blob.
    let resp: serde_json::Value = http
        .post(format!("{url}/api/v1/chunks/missing"))
        .json(&serde_json::json!({
            "blob_hash": novel_hash,
            "chunk_ids": picked_ids.clone(),
        }))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(resp["missing"], serde_json::json!([]));
    let resp = http
        .post(format!("{url}/api/v1/blobs/{novel_hash}/chunks"))
        .json(&serde_json::json!({ "chunk_ids": picked_ids.clone() }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Now a blob made of one staged chunk + one pack chunk: the staged one
    // must not be re-requested.
    let fresh = b"brand new chunk bytes, definitely not in any pack".to_vec();
    let fresh_id = hash_of(&fresh);
    let mut mix = fresh.clone();
    mix.extend_from_slice(&picked[0]);
    let mix_hash = hash_of(&mix);
    let mix_ids = vec![fresh_id.clone(), picked_ids[0].clone()];

    let resp = http
        .post(format!("{url}/api/v1/chunks/{fresh_id}"))
        .body(fresh.clone())
        .send()
        .unwrap();
    assert_eq!(resp.status(), 201);
    let resp: serde_json::Value = http
        .post(format!("{url}/api/v1/chunks/missing"))
        .json(&serde_json::json!({
            "blob_hash": mix_hash,
            "chunk_ids": mix_ids.clone(),
        }))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        resp["missing"],
        serde_json::json!([]),
        "the staged chunk counts as present — nothing left to upload"
    );
    let resp = http
        .post(format!("{url}/api/v1/blobs/{mix_hash}/chunks"))
        .json(&serde_json::json!({ "chunk_ids": mix_ids }))
        .send()
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Both blobs exist, whole and correct; staging is consumed.
    let store = Store::open_chunked(&hub_root).unwrap();
    assert!(store.has_blob(&novel_hash));
    assert!(store.has_blob(&mix_hash));
    store.verify_blob(&novel_hash).unwrap();
    store.verify_blob(&mix_hash).unwrap();
    assert!(
        !hub_root.join("hub-staging").join(&fresh_id).exists(),
        "assembly consumes the staging"
    );
}

#[test]
fn chunk_writes_respect_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let hub = myc_hub::spawn_chunked(dir.path().join("hub"), 0, Some("s3cret".into())).unwrap();
    let http = reqwest::blocking::Client::new();
    let id = blake3::hash(b"a chunk").to_hex().to_string();
    let resp = http
        .post(format!("{}/api/v1/chunks/{id}", hub.url()))
        .body(b"a chunk".to_vec())
        .send()
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = http
        .post(format!("{}/api/v1/chunks/{id}", hub.url()))
        .bearer_auth("s3cret")
        .body(b"a chunk".to_vec())
        .send()
        .unwrap();
    assert_eq!(resp.status(), 201);
}

#[test]
fn capabilities_flag_follows_the_hub_store() {
    let dir = tempfile::tempdir().unwrap();
    let v2 = myc_hub::spawn_chunked(dir.path().join("v2"), 0, None).unwrap();
    let c2 = HubClient::new(&v2.url(), None).unwrap();
    assert!(c2.capabilities().unwrap().chunks);
    assert_eq!(c2.ping().unwrap()["chunks"], true);

    let v1 = myc_hub::spawn(dir.path().join("v1"), 0, None).unwrap();
    let c1 = HubClient::new(&v1.url(), None).unwrap();
    assert!(!c1.capabilities().unwrap().chunks);
}

/// A v1 client store pulling from a chunk-capable hub must go through the
/// plain blob routes and still end up complete (fallback correctness, not
/// just byte savings — this is the compatibility guarantee).
#[test]
fn v1_store_pulls_from_chunked_hub_via_blob_routes() {
    let dir = tempfile::tempdir().unwrap();
    let a = Store::open_chunked(dir.path().join("a")).unwrap();
    let (_, m2) = make_env(&a, "app:3.21", &env_v2());
    let hub = myc_hub::spawn_chunked(dir.path().join("hub"), 0, None).unwrap();
    let client = HubClient::new(&hub.url(), None).unwrap();
    transfer::push(&a, &client, &m2, "app:3.21", transfer::silent()).unwrap();

    let plain = Store::open(dir.path().join("plain")).unwrap();
    let report = transfer::pull(&plain, &client, "app:3.21", transfer::silent()).unwrap();
    assert!(!report.chunked);
    assert!(plain.missing_blobs(&m2).is_empty());
    assert_eq!(plain.resolve("app:3.21").unwrap(), report.manifest_id);
    // And it never grew a pack layer of its own.
    assert!(!plain.root().join("packs").exists());
}
