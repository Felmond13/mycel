//! Guest asset auto-provision, shared by every microVM engine
//! (kvm-microvm on Linux, hvf-microvm/libkrun on macOS).
//!
//! Sources are public (BUSL-1.1) but installs are binaries: when a guest
//! kernel / initramfs is missing at its expected system location, it is
//! downloaded once from the matching GitHub Release into a user cache.
//! Pure decision logic ([`locate_vm_asset`]) is unit-tested on every
//! platform; the HTTP fetch itself is behind `cfg(any(linux, macos))`
//! users only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{BackendError, Result};

/// GitHub repository hosting the releases the guest assets are downloaded
/// from (override with `MYCEL_ASSETS_REPO`).
pub(crate) const ASSETS_REPO: &str = "Felmond13/mycel";
/// Below these sizes an "asset" is certainly truncated or corrupt (the
/// real kernel is ~4.5 MB, the real initramfs ~2.2 MB).
pub(crate) const MIN_KERNEL_BYTES: u64 = 1024 * 1024;
/// The initramfs guard is only consumed by the kvm engine (the libkrun
/// model needs no initramfs — its init ships inside the library).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const MIN_INITRAMFS_BYTES: u64 = 64 * 1024;
/// Generous budget for the ~7 MB of assets on a slow link.
pub(crate) const ASSET_TIMEOUT: Duration = Duration::from_secs(600);
pub(crate) const ASSET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Download cache for auto-provisioned assets (`MYCEL_VM_ASSETS_DIR` or
/// `~/.mycel/vm-assets/`). The system dir (`/usr/local/share/mycel/`) is
/// root-owned and stays the sysadmin's — or `install.sh --with-vm`'s — job.
pub(crate) fn vm_assets_cache() -> PathBuf {
    if let Ok(dir) = std::env::var("MYCEL_VM_ASSETS_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".mycel").join("vm-assets")
}

/// Candidate release URLs for `asset`: the tag matching this build's
/// version first (reproducible), GitHub's `latest/download` shortcut as
/// fallback (the next release after this one, or a rebuilt one).
fn asset_urls(asset: &str) -> Vec<String> {
    let repo = std::env::var("MYCEL_ASSETS_REPO").unwrap_or_else(|_| ASSETS_REPO.into());
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    vec![
        format!("https://github.com/{repo}/releases/download/{tag}/{asset}"),
        format!("https://github.com/{repo}/releases/latest/download/{asset}"),
    ]
}

/// sha256 as lowercase hex — the format of the `.sha256` sidecars published
/// by the release workflow (plain `sha256sum` output).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Download `asset` into `dest` (a `*.part` path), trying each candidate
/// URL in order. Every candidate must clear `min_bytes`, and a published
/// `.sha256` sidecar — when present — must match (no sidecar, no check:
/// pre-sidecar releases still work). Runs on a plain thread: reqwest's
/// blocking client must not execute under an ambient tokio runtime.
pub(crate) fn download_asset(asset: &str, dest: &Path, min_bytes: u64) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(ASSET_CONNECT_TIMEOUT)
        .timeout(ASSET_TIMEOUT)
        .build()
        .map_err(|e| BackendError::Unavailable(format!("asset http client: {e}")))?;
    let mut errors: Vec<String> = Vec::new();
    for url in asset_urls(asset) {
        let fetched = (|| -> Result<Vec<u8>> {
            let resp = client
                .get(&url)
                .send()
                .and_then(|r| r.error_for_status())
                .map_err(|e| BackendError::Unavailable(format!("{url}: {e}")))?;
            let bytes = resp
                .bytes()
                .map_err(|e| BackendError::Unavailable(format!("{url}: body: {e}")))?
                .to_vec();
            if (bytes.len() as u64) < min_bytes {
                return Err(BackendError::Unavailable(format!(
                    "{url}: only {} bytes (expected >= {min_bytes})",
                    bytes.len()
                )));
            }
            // Best-effort integrity against the published sidecar.
            if let Ok(side) = client
                .get(format!("{url}.sha256"))
                .send()
                .and_then(|r| r.error_for_status())
            {
                if let Ok(text) = side.text() {
                    if let Some(expected) = text.split_whitespace().next() {
                        if expected.len() == 64
                            && !expected.eq_ignore_ascii_case(&sha256_hex(&bytes))
                        {
                            return Err(BackendError::Unavailable(format!(
                                "{url}: sha256 mismatch against the published sidecar"
                            )));
                        }
                    }
                }
            }
            Ok(bytes)
        })();
        match fetched {
            Ok(bytes) => {
                std::fs::write(dest, bytes)?;
                return Ok(());
            }
            Err(e) => errors.push(e.to_string()),
        }
    }
    Err(BackendError::Unavailable(format!(
        "cannot download guest asset '{asset}' — {}. Check the network, or build locally: \
         bench/microvm/build_kernel_vmfs.sh && sudo bench/microvm/mkinit-vm.sh --install",
        errors.join("; ")
    )))
}

/// Where an asset can come from, cheapest source first.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AssetSource {
    /// Already at the expected system location.
    System(PathBuf),
    /// Already in the download cache, size-checked.
    Cache(PathBuf),
    /// Nowhere — needs downloading.
    Missing,
}

/// Pure decision used by every asset consumer: expected location first,
/// then a size-checked cache hit.
pub(crate) fn locate_vm_asset(
    expected: &Path,
    cache: &Path,
    asset: &str,
    min_bytes: u64,
) -> AssetSource {
    if expected.is_file() {
        return AssetSource::System(expected.to_path_buf());
    }
    let cached = cache.join(asset);
    let size_ok = std::fs::metadata(&cached)
        .map(|m| m.len() >= min_bytes)
        .unwrap_or(false);
    if size_ok {
        AssetSource::Cache(cached)
    } else {
        AssetSource::Missing
    }
}

/// Effective path for a guest asset, auto-provisioning when missing: the
/// expected system location first, the download cache second, a one-shot
/// download from the GitHub release last (this build's tag, then
/// `latest`). Downloads land in the cache — never in the root-owned
/// system dir. Shared by `run()` paths of every microVM engine.
pub(crate) fn ensure_asset(expected: &Path, asset: &str, min_bytes: u64) -> Result<PathBuf> {
    let cache = vm_assets_cache();
    match locate_vm_asset(expected, &cache, asset, min_bytes) {
        AssetSource::System(path) | AssetSource::Cache(path) => Ok(path),
        AssetSource::Missing => {
            // Callers narrate the download with their own env-var hints.
            std::fs::create_dir_all(&cache)?;
            let dest = cache.join(asset);
            let part = cache.join(format!("{asset}.part"));
            // Dedicated thread: reqwest's blocking client must not run
            // under an ambient tokio runtime (myc-web embeds the engine).
            let asset_owned = asset.to_string();
            let part_for_thread = part.clone();
            let spawned = std::thread::Builder::new()
                .name("myc-vm-assets".into())
                .spawn(move || download_asset(&asset_owned, &part_for_thread, min_bytes))
                .map_err(|e| {
                    BackendError::Unavailable(format!("spawn asset download thread: {e}"))
                })?;
            match spawned.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(BackendError::Unavailable(
                        "asset download thread panicked".into(),
                    ))
                }
            }
            std::fs::rename(&part, &dest)?;
            Ok(dest)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_urls_prefer_the_versioned_tag_then_latest() {
        let urls = asset_urls("vmlinux-vmfs");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("/releases/download/v"), "{urls:?}");
        assert_eq!(
            urls[1],
            "https://github.com/Felmond13/mycel/releases/latest/download/vmlinux-vmfs"
        );
    }

    #[test]
    fn sha256_hex_matches_sha256sum_output() {
        // `echo -n hello | sha256sum` → 2cf24dba5fb0a30e26e83b2ac5b9e29e...
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn locate_prefers_system_then_size_checked_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let sys = tmp.path().join("sys-kernel");
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        // Missing everywhere.
        assert_eq!(
            locate_vm_asset(&sys, &cache, "asset.bin", 4),
            AssetSource::Missing
        );
        // A too-small cache entry is still missing (truncated download).
        std::fs::write(cache.join("asset.bin"), b"abc").unwrap();
        assert_eq!(
            locate_vm_asset(&sys, &cache, "asset.bin", 4),
            AssetSource::Missing
        );
        // A size-checked cache hit counts.
        std::fs::write(cache.join("asset.bin"), b"abcd").unwrap();
        assert_eq!(
            locate_vm_asset(&sys, &cache, "asset.bin", 4),
            AssetSource::Cache(cache.join("asset.bin"))
        );
        // The expected system location always wins.
        std::fs::write(&sys, b"x").unwrap();
        assert_eq!(
            locate_vm_asset(&sys, &cache, "asset.bin", 4),
            AssetSource::System(sys.clone())
        );
    }
}
