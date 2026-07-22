//! `myc deploy`: push an environment into a remote store over any
//! byte-stream transport — in practice the stdin/stdout of an `ssh`
//! subprocess running `myc _serve-deploy` on the target machine.
//!
//! Same negotiation as the hub (`Store::missing_blobs`), different pipe:
//! where `myc push` speaks HTTP to a long-lived hub, `myc deploy` speaks a
//! tiny framed protocol to a one-shot process. Four message kinds, one
//! JSON object per line, raw blob bytes in between (git-pack spirit):
//!
//! ```text
//! local → remote   {"v":1,"ref_name":"web:v2","manifest":{…}}
//! remote → local   {"missing":["<hash>",…]}
//! local → remote   {"hash":"…","size":123,"mode":292}  + 123 raw bytes
//!                  (repeated once per missing blob, in the remote's order)
//! remote → local   {"ok":true,"manifest_id":"myc1-…",
//!                   "blobs_received":3,"bytes_received":456}
//! ```
//!
//! The remote re-hashes every blob as it lands (the store does that on
//! write) and registers the manifest and ref only once every blob is
//! present — the same "never advertise an incomplete environment" ordering
//! `myc push` uses. A second deploy of the same content sends zero blobs.

use crate::transfer::Progress;
use myc_manifest::Manifest;
use myc_store::Store;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    #[error("transport i/o failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("deploy protocol error: {0}")]
    Protocol(String),
    #[error("remote error: {0}")]
    Remote(String),
    #[error("corrupt blob in transfer: expected {expected}, got {actual}")]
    CorruptBlob { expected: String, actual: String },
    #[error(transparent)]
    Store(#[from] myc_store::StoreError),
    #[error(transparent)]
    Manifest(#[from] myc_manifest::ManifestError),
}

pub type Result<T> = std::result::Result<T, DeployError>;

/// First message, local → remote: what is being deployed, and its full
/// manifest (canonical JSON is single-line, so it fits the line framing).
#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub v: u32,
    pub ref_name: String,
    pub manifest: Manifest,
}

/// Remote → local: the subset of the manifest's blobs the remote store
/// does not have. The local side sends exactly these, in this order.
#[derive(Debug, Serialize, Deserialize)]
pub struct MissingReply {
    pub missing: Vec<String>,
}

/// Local → remote, before each blob's raw bytes.
#[derive(Debug, Serialize, Deserialize)]
pub struct BlobHeader {
    pub hash: String,
    pub size: u64,
    /// Permission bits to store the blob inode with (write bits stripped
    /// server-side; an optimization, never trusted for content).
    pub mode: u32,
}

/// Final message, remote → local. Also sent early (`ok: false`) when the
/// remote fails mid-conversation, so the local side gets a real reason
/// instead of a broken pipe.
#[derive(Debug, Serialize, Deserialize)]
pub struct Outcome {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub blobs_received: usize,
    #[serde(default)]
    pub bytes_received: u64,
}

/// Outcome of a `myc deploy`, local side.
#[derive(Debug)]
pub struct DeployReport {
    pub manifest_id: String,
    pub ref_name: String,
    /// Total blobs referenced by the manifest.
    pub blobs_total: usize,
    /// Blobs the remote was missing (actually sent).
    pub blobs_sent: usize,
    pub bytes_total: u64,
    pub bytes_sent: u64,
    pub elapsed: Duration,
}

fn send_line<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let mut line =
        serde_json::to_vec(msg).map_err(|e| DeployError::Protocol(format!("encode: {e}")))?;
    line.push(b'\n');
    w.write_all(&line)?;
    Ok(())
}

/// Read one JSON line as `T`. When the other side sent an error `Outcome`
/// instead, surface its message rather than a parse failure.
fn recv_line<R: BufRead, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<T> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Err(DeployError::Protocol(
            "connection closed mid-conversation".into(),
        ));
    }
    if let Ok(msg) = serde_json::from_str::<T>(&line) {
        return Ok(msg);
    }
    if let Ok(outcome) = serde_json::from_str::<Outcome>(&line) {
        if !outcome.ok {
            return Err(DeployError::Remote(
                outcome.error.unwrap_or_else(|| "unknown".into()),
            ));
        }
    }
    Err(DeployError::Protocol(format!(
        "unexpected message: {}",
        line.trim()
    )))
}

/// Local half of a deploy: send `manifest` under `ref_name` through the
/// transport, upload only what the remote reports missing, return the
/// verified outcome. Safe to re-run; a second deploy sends nothing.
pub fn push_deploy<R: BufRead, W: Write>(
    store: &Store,
    manifest: &Manifest,
    ref_name: &str,
    to_remote: &mut W,
    from_remote: &mut R,
    progress: Progress<'_>,
) -> Result<DeployReport> {
    let start = Instant::now();
    let blobs = manifest.referenced_blobs();
    let bytes_total: u64 = blobs.values().sum();
    let blobs_total = blobs.len();

    send_line(
        to_remote,
        &Hello {
            v: PROTOCOL_VERSION,
            ref_name: ref_name.to_string(),
            manifest: manifest.clone(),
        },
    )?;
    to_remote.flush()?;

    // The whole point: ask once, send only the delta.
    let missing: MissingReply = recv_line(from_remote)?;
    let mut bytes_sent = 0u64;
    for (i, hash) in missing.missing.iter().enumerate() {
        let path = store.blob(hash)?;
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        send_line(
            to_remote,
            &BlobHeader {
                hash: hash.clone(),
                size,
                mode: crate::client::blob_mode(&path),
            },
        )?;
        let mut file = std::fs::File::open(&path)?;
        std::io::copy(&mut file, to_remote)?;
        bytes_sent += size;
        progress(i + 1, missing.missing.len());
    }
    to_remote.flush()?;

    let outcome: Outcome = recv_line(from_remote)?;
    if !outcome.ok {
        return Err(DeployError::Remote(
            outcome.error.unwrap_or_else(|| "unknown".into()),
        ));
    }
    let manifest_id = outcome
        .manifest_id
        .ok_or_else(|| DeployError::Protocol("outcome without manifest id".into()))?;
    Ok(DeployReport {
        manifest_id,
        ref_name: ref_name.to_string(),
        blobs_total,
        blobs_sent: missing.missing.len(),
        bytes_total,
        bytes_sent,
        elapsed: start.elapsed(),
    })
}

/// Remote half (`myc _serve-deploy`): receive a deploy on `input`/`output`
/// (in practice stdin/stdout of the ssh session). On failure an error
/// `Outcome` is emitted before returning, so the local side sees the reason.
pub fn serve_deploy<R: BufRead, W: Write>(
    store: &Store,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    match serve_inner(store, input, output) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = send_line(
                output,
                &Outcome {
                    ok: false,
                    manifest_id: None,
                    error: Some(e.to_string()),
                    blobs_received: 0,
                    bytes_received: 0,
                },
            );
            let _ = output.flush();
            Err(e)
        }
    }
}

fn serve_inner<R: BufRead, W: Write>(store: &Store, input: &mut R, output: &mut W) -> Result<()> {
    let hello: Hello = recv_line(input)?;
    if hello.v != PROTOCOL_VERSION {
        return Err(DeployError::Protocol(format!(
            "protocol version {} not supported (this side speaks {PROTOCOL_VERSION}) — \
             upgrade myc on the older machine",
            hello.v
        )));
    }
    if !crate::valid_ref_name(&hello.ref_name) {
        return Err(DeployError::Protocol(format!(
            "invalid ref name '{}'",
            hello.ref_name
        )));
    }
    hello.manifest.validate()?;

    let missing = store.missing_blobs(&hello.manifest);
    send_line(
        output,
        &MissingReply {
            missing: missing.clone(),
        },
    )?;
    output.flush()?;

    let mut bytes_received = 0u64;
    for expected in &missing {
        let header: BlobHeader = recv_line(input)?;
        if header.hash != *expected {
            return Err(DeployError::Protocol(format!(
                "expected blob {expected}, got {}",
                header.hash
            )));
        }
        // The store re-hashes the stream; a corrupt transfer lands under its
        // true hash (unreferenced -> GC fodder) and is reported.
        let mut limited = input.take(header.size);
        let (actual, size) = store.put_blob_with_mode(&mut limited, header.mode)?;
        if size != header.size {
            return Err(DeployError::Protocol(format!(
                "blob {expected} truncated: {size} of {} bytes",
                header.size
            )));
        }
        if actual != header.hash {
            return Err(DeployError::CorruptBlob {
                expected: header.hash,
                actual,
            });
        }
        bytes_received += size;
    }

    // Manifest and ref last: this store never advertises an environment
    // whose blobs are not yet all present.
    let manifest_id = store.put_manifest(&hello.manifest)?;
    store.set_ref(&hello.ref_name, &manifest_id)?;

    send_line(
        output,
        &Outcome {
            ok: true,
            manifest_id: Some(manifest_id),
            error: None,
            blobs_received: missing.len(),
            bytes_received,
        },
    )?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer;
    use myc_manifest::{Entry, EntryKind, RuntimeConfig, SCHEMA_VERSION};
    use std::io::BufReader;

    fn entry(path: &str, hash: &str, size: u64) -> Entry {
        Entry {
            path: path.into(),
            kind: EntryKind::File,
            mode: 0o644,
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

    /// Run a full deploy between two stores over in-process pipes.
    fn deploy(local: &Store, remote: &Store, m: &Manifest, ref_name: &str) -> DeployReport {
        let (serve_in, mut push_out) = std::io::pipe().unwrap();
        let (push_in, mut serve_out) = std::io::pipe().unwrap();
        std::thread::scope(|scope| {
            let server = scope
                .spawn(move || serve_deploy(remote, &mut BufReader::new(serve_in), &mut serve_out));
            let report = push_deploy(
                local,
                m,
                ref_name,
                &mut push_out,
                &mut BufReader::new(push_in),
                transfer::silent(),
            )
            .expect("push side failed");
            server.join().unwrap().expect("serve side failed");
            report
        })
    }

    #[test]
    fn first_deploy_sends_everything_second_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let local = Store::open(dir.path().join("local")).unwrap();
        let remote = Store::open(dir.path().join("remote")).unwrap();

        let (ha, _) = local.put_blob(&mut &b"content a"[..]).unwrap();
        let (hb, _) = local.put_blob(&mut &b"content b"[..]).unwrap();
        let m = manifest("app:v1", vec![entry("/a", &ha, 9), entry("/b", &hb, 9)]);
        let id = local.put_manifest(&m).unwrap();

        let r1 = deploy(&local, &remote, &m, "app:v1");
        assert_eq!(r1.blobs_total, 2);
        assert_eq!(r1.blobs_sent, 2);
        assert_eq!(r1.bytes_sent, 18);
        assert_eq!(r1.manifest_id, id);
        // The remote is complete and resolvable under the deployed name.
        assert_eq!(remote.resolve("app:v1").unwrap(), id);
        let got = remote.get_manifest(&id).unwrap();
        assert!(remote.missing_blobs(&got).is_empty());
        remote.verify_blob(&ha).unwrap();
        remote.verify_blob(&hb).unwrap();

        let r2 = deploy(&local, &remote, &m, "app:v1");
        assert_eq!(r2.blobs_sent, 0);
        assert_eq!(r2.bytes_sent, 0);
    }

    #[test]
    fn sibling_deploy_sends_only_the_delta() {
        let dir = tempfile::tempdir().unwrap();
        let local = Store::open(dir.path().join("local")).unwrap();
        let remote = Store::open(dir.path().join("remote")).unwrap();

        let (ha, _) = local.put_blob(&mut &b"shared file"[..]).unwrap();
        let (hb, _) = local.put_blob(&mut &b"only in v1"[..]).unwrap();
        let (hc, _) = local.put_blob(&mut &b"only in v2"[..]).unwrap();
        let v1 = manifest("app:v1", vec![entry("/a", &ha, 11), entry("/b", &hb, 10)]);
        let v2 = manifest("app:v2", vec![entry("/a", &ha, 11), entry("/c", &hc, 10)]);
        local.put_manifest(&v1).unwrap();
        local.put_manifest(&v2).unwrap();

        let r1 = deploy(&local, &remote, &v1, "app:v1");
        assert_eq!(r1.blobs_sent, 2);
        // v2 shares /a with v1: only /c crosses the wire.
        let r2 = deploy(&local, &remote, &v2, "app:v2");
        assert_eq!(r2.blobs_total, 2);
        assert_eq!(r2.blobs_sent, 1);
        assert_eq!(r2.bytes_sent, 10);
        assert!(remote.resolve("app:v1").is_ok());
        assert!(remote.resolve("app:v2").is_ok());
    }

    #[test]
    fn message_encode_decode_roundtrip() {
        let hello = Hello {
            v: 1,
            ref_name: "web:v2".into(),
            manifest: manifest("web:v2", vec![]),
        };
        let mut buf = Vec::new();
        send_line(&mut buf, &hello).unwrap();
        assert_eq!(buf.iter().filter(|b| **b == b'\n').count(), 1);
        let back: Hello = recv_line(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(back.ref_name, "web:v2");
        assert_eq!(back.v, 1);

        let header = BlobHeader {
            hash: "ab".repeat(32),
            size: 42,
            mode: 0o755,
        };
        let mut buf = Vec::new();
        send_line(&mut buf, &header).unwrap();
        let back: BlobHeader = recv_line(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(back.hash, header.hash);
        assert_eq!(back.size, 42);
        assert_eq!(back.mode, 0o755);
    }

    #[test]
    fn error_outcome_surfaces_the_remote_reason() {
        let mut buf = Vec::new();
        send_line(
            &mut buf,
            &Outcome {
                ok: false,
                manifest_id: None,
                error: Some("disk full".into()),
                blobs_received: 0,
                bytes_received: 0,
            },
        )
        .unwrap();
        let err = recv_line::<_, MissingReply>(&mut std::io::Cursor::new(&buf)).unwrap_err();
        assert!(matches!(err, DeployError::Remote(ref m) if m == "disk full"));
    }

    #[test]
    fn unsupported_version_is_rejected_with_an_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let remote = Store::open(dir.path().join("remote")).unwrap();
        let mut input = Vec::new();
        send_line(
            &mut input,
            &Hello {
                v: 99,
                ref_name: "x:1".into(),
                manifest: manifest("x:1", vec![]),
            },
        )
        .unwrap();
        let mut output = Vec::new();
        let err =
            serve_deploy(&remote, &mut std::io::Cursor::new(&input), &mut output).unwrap_err();
        assert!(matches!(err, DeployError::Protocol(_)));
        let outcome: Outcome =
            recv_line(&mut std::io::Cursor::new(&output)).expect("an error outcome was written");
        assert!(!outcome.ok);
        assert!(outcome.error.unwrap().contains("version 99"));
    }

    #[test]
    fn corrupt_blob_is_rejected_and_ref_not_set() {
        let dir = tempfile::tempdir().unwrap();
        let remote = Store::open(dir.path().join("remote")).unwrap();
        let real_hash = blake3::hash(b"real content").to_hex().to_string();
        let m = manifest("bad:1", vec![entry("/f", &real_hash, 12)]);

        let mut input = Vec::new();
        send_line(
            &mut input,
            &Hello {
                v: PROTOCOL_VERSION,
                ref_name: "bad:1".into(),
                manifest: m,
            },
        )
        .unwrap();
        send_line(
            &mut input,
            &BlobHeader {
                hash: real_hash,
                size: 12,
                mode: 0o644,
            },
        )
        .unwrap();
        input.extend_from_slice(b"wrong  bytes"); // 12 bytes, wrong content
        let mut output = Vec::new();
        let err =
            serve_deploy(&remote, &mut std::io::Cursor::new(&input), &mut output).unwrap_err();
        assert!(matches!(err, DeployError::CorruptBlob { .. }));
        assert!(remote.resolve("bad:1").is_err(), "ref must not be set");
    }
}
