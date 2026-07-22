//! Sharing endpoints: download / upload portable `.mycel` archives and
//! exchange environments with a hub. This is the web face of
//! `myc export`, `myc import`, `myc push` and `myc pull --from`, built on
//! the same library code (`myc_store::archive`, `myc-hub`).

use crate::{
    bad_request, blocking, check_reference, internal, not_found, open_store, resolve,
    resolve_installed, ApiError, ApiResult, App,
};
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use myc_hub::client::HubError;
use myc_store::archive::{export_archive, import_archive, ArchiveError};
use serde::Deserialize;
use serde_json::json;
use std::io::{Seek, SeekFrom};
use tokio::io::AsyncWriteExt;

/// Upload cap for `POST /api/import` (a `.mycel` archive is gzip-compressed;
/// 8 GiB covers even very large environments with room to spare).
pub(crate) const IMPORT_BODY_LIMIT: usize = 8 * 1024 * 1024 * 1024;

/// Download filename for an environment: `docker.io/library/redis:7-alpine`
/// becomes `redis-7-alpine.mycel`.
fn download_name(manifest_name: &str) -> String {
    let short = manifest_name
        .strip_prefix("docker.io/library/")
        .unwrap_or(manifest_name);
    let mut out = String::new();
    for c in short.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || c == '.' {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches(['-', '.']);
    let base = if trimmed.is_empty() {
        "environment"
    } else {
        trimmed
    };
    format!("{base}.mycel")
}

/// `GET /api/export/{ref}`: pack the environment (manifest + every blob)
/// into a `.mycel` archive and stream it as a download.
pub(crate) async fn export_env(
    State(app): State<App>,
    UrlPath(reference): UrlPath<String>,
) -> Result<Response, ApiError> {
    check_reference(&reference)?;
    // Build the archive into an anonymous temp file on the blocking pool
    // (it is unlinked from birth, so nothing leaks even on a crash), then
    // stream it out without holding it in memory.
    let (file, name, size) = blocking(move || {
        let store = open_store(&app.root)?;
        let (_, manifest) = resolve(&store, &reference)?;
        let mut file = tempfile::tempfile().map_err(internal)?;
        export_archive(&store, &manifest, &mut file).map_err(|e| match e {
            ArchiveError::MissingBlobs(n) => bad_request(format!(
                "cannot share '{}' — {n} of its files are not on this machine yet \
                 (run `myc pull` to complete it first)",
                manifest.name
            )),
            other => internal(other),
        })?;
        let size = file.metadata().map_err(internal)?.len();
        file.seek(SeekFrom::Start(0)).map_err(internal)?;
        Ok((file, download_name(&manifest.name), size))
    })
    .await?;

    let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file));
    let headers = [
        (header::CONTENT_TYPE, "application/gzip".to_string()),
        (header::CONTENT_LENGTH, size.to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\""),
        ),
    ];
    Ok((headers, Body::from_stream(stream)).into_response())
}

fn import_error(e: ArchiveError) -> ApiError {
    match e {
        ArchiveError::Store(inner) => internal(inner),
        other => bad_request(format!("import failed: {other}")),
    }
}

/// `POST /api/import`: the request body is a `.mycel` archive
/// (`application/octet-stream`); it is spooled to a temp file, verified
/// blob by blob and imported into the store.
pub(crate) async fn import_upload(State(app): State<App>, body: Body) -> ApiResult {
    // Spool to an anonymous temp file first — archives can be large and the
    // tar reader wants a blocking `Read` anyway.
    let spool = tempfile::tempfile().map_err(internal)?;
    let mut spool = tokio::fs::File::from_std(spool);
    let mut stream = body.into_data_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| bad_request(format!("upload interrupted: {e}")))?;
        written += chunk.len() as u64;
        if written > IMPORT_BODY_LIMIT as u64 {
            return Err(bad_request(
                "this file is larger than the 8 GiB import limit",
            ));
        }
        spool.write_all(&chunk).await.map_err(internal)?;
    }
    if written == 0 {
        return Err(bad_request(
            "no file received — send the .mycel file as the request body",
        ));
    }
    spool.flush().await.map_err(internal)?;
    let mut file = spool.into_std().await;

    blocking(move || {
        file.seek(SeekFrom::Start(0)).map_err(internal)?;
        let store = open_store(&app.root)?;
        let report = import_archive(&store, file).map_err(import_error)?;
        Ok(Json(json!({
            "id": report.manifest_id,
            "name": report.name,
            "refs": [report.name],
            "blobs": report.blobs,
        })))
    })
    .await
}

// ---- hub push / pull ----

#[derive(Deserialize)]
pub(crate) struct HubBody {
    #[serde(rename = "ref")]
    reference: String,
    url: String,
    #[serde(default)]
    token: Option<String>,
}

fn check_hub_url(url: &str) -> Result<(), ApiError> {
    let ok = (url.starts_with("http://") || url.starts_with("https://")) && url.len() <= 2048;
    if ok {
        Ok(())
    } else {
        Err(bad_request(
            "hub URL must start with http:// or https:// (e.g. http://hub:9600)",
        ))
    }
}

fn hub_error(e: HubError) -> ApiError {
    match e {
        HubError::Unreachable { .. } | HubError::Rejected { .. } | HubError::CorruptBlob { .. } => {
            bad_request(e.to_string())
        }
        HubError::NotFound { .. } => not_found(e.to_string()),
        other => internal(other),
    }
}

/// Names to try on a hub: the reference as given, then its canonical
/// docker-style form (`alpine:3.20` → `docker.io/library/alpine:3.20`).
fn hub_candidates(reference: &str) -> Vec<String> {
    let mut out = vec![reference.to_string()];
    if let Ok(image) = myc_oci::ImageReference::parse(reference) {
        let canonical = image.canonical_name();
        if canonical != reference {
            out.push(canonical);
        }
    }
    out
}

/// `POST /api/hub/push`: send an installed environment to a hub — only the
/// blobs the hub is missing cross the wire.
pub(crate) async fn hub_push(State(app): State<App>, Json(body): Json<HubBody>) -> ApiResult {
    check_reference(&body.reference)?;
    check_hub_url(&body.url)?;
    blocking(move || {
        let store = open_store(&app.root)?;
        let id = resolve_installed(&store, &body.reference)?;
        let manifest = store.get_manifest(&id).map_err(internal)?;
        let missing = store.missing_blobs(&manifest);
        if !missing.is_empty() {
            return Err(bad_request(format!(
                "'{}' is incomplete on this machine ({} files missing) — complete it before sharing",
                body.reference,
                missing.len()
            )));
        }
        let client =
            myc_hub::HubClient::new(&body.url, body.token.clone()).map_err(internal)?;
        let report = myc_hub::transfer::push(
            &store,
            &client,
            &manifest,
            &body.reference,
            myc_hub::transfer::silent(),
        )
        .map_err(hub_error)?;
        Ok(Json(json!({
            "ref": report.ref_name,
            "manifest_id": report.manifest_id,
            "blobs_total": report.blobs_total,
            "blobs_uploaded": report.blobs_uploaded,
            "blobs_already_present": report.blobs_total - report.blobs_uploaded,
            "bytes_total": report.bytes_total,
            "bytes_uploaded": report.bytes_uploaded,
            "elapsed_ms": report.elapsed.as_millis() as u64,
        })))
    })
    .await
}

/// `POST /api/hub/pull`: fetch an environment from a hub — only the blobs
/// this machine is missing are downloaded.
pub(crate) async fn hub_pull(State(app): State<App>, Json(body): Json<HubBody>) -> ApiResult {
    check_reference(&body.reference)?;
    check_hub_url(&body.url)?;
    blocking(move || {
        let store = open_store(&app.root)?;
        let client = myc_hub::HubClient::new(&body.url, body.token.clone()).map_err(internal)?;
        let mut candidates = hub_candidates(&body.reference).into_iter().peekable();
        let report = loop {
            let name = candidates.next().expect("at least one candidate");
            match myc_hub::transfer::pull(&store, &client, &name, myc_hub::transfer::silent()) {
                Ok(r) => break r,
                // Short name not on the hub: try the canonical form too.
                Err(HubError::NotFound { .. }) if candidates.peek().is_some() => {}
                Err(e) => return Err(hub_error(e)),
            }
        };
        // Make the name the user typed resolvable locally next time.
        if store.resolve(&body.reference).is_err() {
            let _ = store.set_ref(&body.reference, &report.manifest_id);
        }
        Ok(Json(json!({
            "ref": body.reference,
            "manifest_id": report.manifest_id,
            "blobs_total": report.blobs_total,
            "blobs_downloaded": report.blobs_downloaded,
            "blobs_already_present": report.blobs_total - report.blobs_downloaded,
            "bytes_total": report.bytes_total,
            "bytes_downloaded": report.bytes_downloaded,
            "elapsed_ms": report.elapsed.as_millis() as u64,
        })))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::download_name;

    #[test]
    fn download_names_are_clean() {
        assert_eq!(
            download_name("docker.io/library/redis:7-alpine"),
            "redis-7-alpine.mycel"
        );
        assert_eq!(
            download_name("docker.io/library/alpine:3.20"),
            "alpine-3.20.mycel"
        );
        assert_eq!(
            download_name("ghcr.io/owner/app:v1"),
            "ghcr.io-owner-app-v1.mycel"
        );
        assert_eq!(download_name("///"), "environment.mycel");
    }
}
