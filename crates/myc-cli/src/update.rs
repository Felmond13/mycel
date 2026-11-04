//! Self-update: `myc update` and the doctor "update available" line.
//!
//! The binary distribution lives on GitHub Releases (one asset per platform,
//! `myc-linux-x86_64` / `myc-windows-x86_64.exe`, each with a `.sha256`
//! sidecar in plain `sha256sum` format published by the release workflow).
//! Updating means:
//!
//! 1. GET the latest release from the API (`MYCEL_UPDATE_API` overrides the
//!    endpoint for tests), 5 s budget — this is also what `myc doctor` does,
//!    caching the answer for 24 h in `<store>/update-check.json` so an
//!    offline doctor is not slowed down by a network timeout.
//! 2. Compare the release tag (`vX.Y.Z`) with `CARGO_PKG_VERSION` — a small
//!    hand-rolled semver parse (three numeric components; a pre-release
//!    suffix like `-rc1` is ignored), no external crate.
//! 3. Download the platform asset AND its sidecar, verify sha256 — a
//!    missing or mismatched sidecar aborts the update, no exceptions.
//! 4. Swap atomically: Unix renames `<exe>.new` over the running binary
//!    (same filesystem, the running process keeps its inode). Windows
//!    cannot replace a running .exe, so the new one is staged as
//!    `myc-new.exe` beside the current one with the exact move command.
//!
//! No telemetry, no automatic checks anywhere outside `doctor` and `update`.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
#[cfg(any(windows, test))]
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Latest-release API endpoint (override with `MYCEL_UPDATE_API`, e.g. to
/// point at a local fixture server in tests).
const DEFAULT_API_URL: &str = "https://api.github.com/repos/Felmond13/mycel/releases/latest";

/// Release asset for the running platform (must match .github/workflows/release.yml).
#[cfg(windows)]
pub const ASSET_NAME: &str = "myc-windows-x86_64.exe";
#[cfg(not(windows))]
pub const ASSET_NAME: &str = "myc-linux-x86_64";

/// Budget for the release-API check — `doctor` must never hang on it.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// Budget for the binary download itself (~12 MB on a slow link).
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Where doctor caches the last check (`<store>/update-check.json`).
pub const CACHE_FILE: &str = "update-check.json";
/// A cached check answers for 24 h.
pub const CACHE_TTL_SECS: u64 = 24 * 3600;

// ----------------------------------------------------------------------
// Version parsing / comparison (no semver crate)
// ----------------------------------------------------------------------

/// A release version: exactly `major.minor.patch`. A pre-release suffix
/// (`-rc1`, `-beta.2`) is accepted by [`Version::parse`] and ignored —
/// GitHub's `releases/latest` endpoint never serves pre-releases anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// Parse `vX.Y.Z` / `X.Y.Z` (whitespace tolerated, optional pre-release
    /// or build suffix dropped). Anything else — fewer or more than three
    /// numeric components, non-numeric parts, empty — is `None`.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
        // Drop any "-rc1" / "+build" suffix before splitting the core.
        let core = s.split(['-', '+']).next().unwrap_or(s);
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None; // "1.2.3.4" is not our version scheme
        }
        Some(Version {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

// ----------------------------------------------------------------------
// Release API response (fixture-testable, no network in the parse itself)
// ----------------------------------------------------------------------

/// The one release the API told us about.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// Raw tag, e.g. `v0.4.0` (kept verbatim for messages).
    pub tag: String,
    /// Parsed version of the tag; `None` when the tag is not `vX.Y.Z`
    /// (`nightly`, `latest`, …) — nothing to update to, then.
    pub version: Option<Version>,
    /// `(asset name, download URL)` pairs, straight from the API.
    pub assets: Vec<(String, String)>,
}

impl Release {
    pub fn asset_url(&self, name: &str) -> Option<&str> {
        self.assets
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, url)| url.as_str())
    }
}

/// Parse a GitHub `releases/latest` JSON body (the exact shape the real API
/// returns; unit tests feed a captured fixture). `None` when the body is
/// not JSON or carries no `tag_name`.
pub fn parse_release(json: &str) -> Option<Release> {
    let v: Value = serde_json::from_str(json).ok()?;
    let tag = v.get("tag_name")?.as_str()?.to_string();
    let version = Version::parse(&tag);
    let assets = v
        .get("assets")?
        .as_array()?
        .iter()
        .filter_map(|a| {
            let name = a.get("name")?.as_str()?.to_string();
            let url = a.get("browser_download_url")?.as_str()?.to_string();
            Some((name, url))
        })
        .collect();
    Some(Release {
        tag,
        version,
        assets,
    })
}

// ----------------------------------------------------------------------
// sha256 sidecar (sha256sum format: "<hex>  <filename>")
// ----------------------------------------------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Extract the expected digest from a `.sha256` sidecar: the first
/// whitespace-separated token, which must be 64 hex chars (case-insensitive
/// — normalized to lowercase). `None` when the sidecar is garbage.
pub fn parse_sidecar(text: &str) -> Option<String> {
    let token = text.split_whitespace().next()?;
    if token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(token.to_ascii_lowercase())
    } else {
        None
    }
}

/// Verify downloaded `bytes` against a sidecar's expected digest.
pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = sha256_hex(bytes);
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        bail!(
            "sha256 mismatch: asset hashes to {actual}, the .sha256 sidecar says {expected} \
             — the download was discarded"
        );
    }
}

// ----------------------------------------------------------------------
// Swapping the binary in (no network: tested with local files)
// ----------------------------------------------------------------------

/// Where a verified update landed after [`install_asset`].
#[derive(Debug, PartialEq)]
pub enum Staged {
    /// Unix: renamed over the old binary — the update is live.
    /// Only constructed on Unix targets.
    #[cfg_attr(windows, allow(dead_code))]
    Replaced(PathBuf),
    /// Windows: written as `myc-new.exe` beside the running binary; the
    /// caller prints the swap command (a running .exe cannot be replaced).
    /// Only constructed on Windows targets.
    #[cfg_attr(not(windows), allow(dead_code))]
    StagedBeside(PathBuf),
}

/// Verify `bytes` against `expected_sha`, then put them where the platform
/// allows: Unix renames `<exe>.new` over `exe` (atomic, same filesystem —
/// the running process keeps its old inode); Windows stages `myc-new.exe`
/// next to `exe`. On any failure the current binary is left untouched and
/// no leftover `.new` file remains.
pub fn install_asset(bytes: &[u8], expected_sha: &str, exe: &Path) -> Result<Staged> {
    verify_sha256(bytes, expected_sha)?;
    #[cfg(unix)]
    {
        let name = exe
            .file_name()
            .and_then(|n| n.to_str())
            .context("current binary path has no file name")?;
        let new_exe = exe
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{name}.new"));
        let install = || -> Result<Staged> {
            std::fs::write(&new_exe, bytes)
                .with_context(|| format!("cannot write {}", new_exe.display()))?;
            set_executable(&new_exe, exe)?;
            std::fs::rename(&new_exe, exe).with_context(|| {
                format!("cannot move {} over {}", new_exe.display(), exe.display())
            })?;
            Ok(Staged::Replaced(exe.to_path_buf()))
        };
        match install() {
            Ok(staged) => Ok(staged),
            Err(e) => {
                let _ = std::fs::remove_file(&new_exe); // no leftovers
                Err(e)
            }
        }
    }
    #[cfg(windows)]
    {
        let dir = exe.parent().unwrap_or_else(|| Path::new("."));
        let staged = dir.join("myc-new.exe");
        std::fs::write(&staged, bytes)
            .with_context(|| format!("cannot write {}", staged.display()))?;
        Ok(Staged::StagedBeside(staged))
    }
}

/// Preserve the current binary's mode, plus the executable bits.
#[cfg(unix)]
fn set_executable(new_exe: &Path, exe: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(exe)
        .map(|m| m.permissions().mode())
        .unwrap_or(0o644);
    std::fs::set_permissions(new_exe, std::fs::Permissions::from_mode(mode | 0o755))
        .with_context(|| format!("cannot chmod {}", new_exe.display()))
}

// ----------------------------------------------------------------------
// The 24 h check cache (pure parse/format + one file of IO)
// ----------------------------------------------------------------------

/// Serialize a cache entry (plain JSON, human-readable).
pub fn cache_contents(checked_at: u64, latest: &str) -> String {
    format!("{{\"checked_at\": {checked_at}, \"latest\": \"{latest}\"}}\n")
}

/// Parse a cache entry back. `None` on anything malformed — a corrupt cache
/// simply means "check the network".
pub fn parse_cache(raw: &str) -> Option<(u64, String)> {
    let v: Value = serde_json::from_str(raw).ok()?;
    Some((
        v.get("checked_at")?.as_u64()?,
        v.get("latest")?.as_str()?.to_string(),
    ))
}

/// The latest known version for `doctor`: a fresh (<24 h) cache first, the
/// network when the cache is stale or absent (a successful network answer
/// refreshes the cache), a stale cache as last resort when offline. `None`
/// when nothing at all is known — doctor then says so instead of guessing.
pub fn latest_known(store_root: &Path, now_unix: u64) -> Option<Version> {
    let cache = store_root.join(CACHE_FILE);
    let cached = std::fs::read_to_string(&cache)
        .ok()
        .and_then(|r| parse_cache(&r));
    let fresh = cached
        .as_ref()
        .filter(|(at, _)| now_unix.saturating_sub(*at) < CACHE_TTL_SECS)
        .and_then(|(_, v)| Version::parse(v));
    if let Some(v) = fresh {
        return Some(v);
    }
    match fetch_latest() {
        Ok(release) => {
            let v = release.version?;
            let _ = std::fs::create_dir_all(store_root);
            let _ = std::fs::write(&cache, cache_contents(now_unix, &v.to_string()));
            Some(v)
        }
        // Offline with a stale cache: still more truthful than "unknown".
        Err(_) => cached.and_then(|(_, v)| Version::parse(&v)),
    }
}

/// The single line `myc doctor` prints about release freshness.
pub fn doctor_line(current: &str, latest: Option<Version>) -> String {
    let current = Version::parse(current);
    match (current, latest) {
        (Some(cur), Some(lat)) if lat > cur => {
            format!("update available: {cur} → {lat} (myc update)")
        }
        (Some(cur), Some(_)) => format!("up to date ({cur})"),
        (Some(cur), None) => format!("up to date ({cur}) — could not check for updates (offline?)"),
        (None, _) => {
            "version unknown (custom build?) — cannot compare with the latest release".to_string()
        }
    }
}

// ----------------------------------------------------------------------
// Windows routing (pure decision, unit-tested on Linux too)
// ----------------------------------------------------------------------

/// How the Windows preflight routes `myc update` before clap sees it.
/// (Compiled for Windows and for tests — the Linux binary never routes.)
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateRoute {
    /// Run the native Windows self-update (`--host`, or help requests —
    /// clap then prints the documented behavior).
    Native,
    /// Proxy to the Linux twin inside WSL: the twin (the real engine)
    /// atomically updates its own `~/.mycel-bin/myc`.
    Twin,
}

/// `--host`/`--help`/`-h` anywhere in the arguments mean native; every
/// other `myc update …` goes to the twin.
#[cfg(any(windows, test))]
pub fn route_windows_update(args: &[OsString]) -> UpdateRoute {
    let native = ["--host", "--help", "-h"].iter().any(|flag| {
        args.iter()
            .any(|a| a.to_str().map(|s| s == *flag).unwrap_or(false))
    });
    if native {
        UpdateRoute::Native
    } else {
        UpdateRoute::Twin
    }
}

// ----------------------------------------------------------------------
// Network
// ----------------------------------------------------------------------

fn api_url() -> String {
    std::env::var("MYCEL_UPDATE_API")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_API_URL.to_string())
}

fn client(timeout: Duration, connect_timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(timeout)
        .user_agent(format!(
            "myc/{} (+https://github.com/Felmond13/mycel)",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .map_err(|e| anyhow!("cannot build http client: {e}"))
}

/// Fetch the latest release from the API (`MYCEL_UPDATE_API` wins).
pub fn fetch_latest() -> Result<Release> {
    let client = client(CHECK_TIMEOUT, CHECK_TIMEOUT)?;
    let url = api_url();
    let response = client
        .get(&url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| anyhow!("{url}: {e}"))?;
    let body = response.text().map_err(|e| anyhow!("{url}: body: {e}"))?;
    parse_release(&body)
        .ok_or_else(|| anyhow!("{url}: response has no tag_name — not a release API?"))
}

fn download(url: &str) -> Result<Vec<u8>> {
    let client = client(DOWNLOAD_TIMEOUT, DOWNLOAD_CONNECT_TIMEOUT)?;
    let response = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| anyhow!("{url}: {e}"))?;
    response
        .bytes()
        .map(|b| b.to_vec())
        .map_err(|e| anyhow!("{url}: body: {e}"))
}

// ----------------------------------------------------------------------
// The command
// ----------------------------------------------------------------------

/// `myc update`: check, download, verify, swap. `_host` only matters on
/// Windows, where the preflight has already routed `--host` here to update
/// myc.exe itself instead of the WSL twin (on Linux there is one binary).
pub fn run(_host: bool) -> Result<i32> {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).ok_or_else(|| {
        anyhow!(
            "this build reports version '{}' — not X.Y.Z",
            env!("CARGO_PKG_VERSION")
        )
    })?;

    // An explicit update always asks the network: a 24 h-old cache must not
    // stand between the user and a release published an hour ago.
    let release = fetch_latest()?;
    let Some(latest) = release.version else {
        println!(
            "latest release tag is '{}' — not a versioned release, nothing to update to",
            release.tag
        );
        return Ok(0);
    };
    if latest <= current {
        println!("myc {current} is up to date (latest release: {latest})");
        return Ok(0);
    }

    let asset_url = release.asset_url(ASSET_NAME).ok_or_else(|| {
        anyhow!(
            "release {0} has no '{ASSET_NAME}' asset for this platform \
             (assets: {1})",
            release.tag,
            release
                .assets
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    // The sidecar is mandatory for a binary update: no verified sha256,
    // no swap — fetch it first so nothing is downloaded when it is absent.
    let sidecar_url = format!("{asset_url}.sha256");
    println!("myc {current} → {latest}: checking {sidecar_url}");
    let sidecar = download(&sidecar_url)
        .with_context(|| format!("no sha256 sidecar for '{ASSET_NAME}' — update refused"))?;
    let expected = parse_sidecar(&String::from_utf8_lossy(&sidecar)).ok_or_else(|| {
        anyhow!("the .sha256 sidecar is malformed — update refused (server-side problem?)")
    })?;

    println!("downloading {asset_url}…");
    let bytes = download(asset_url)?;
    println!(
        "downloaded {} ({:.1} MB), verifying sha256…",
        ASSET_NAME,
        bytes.len() as f64 / 1e6
    );

    // Follow symlinks (e.g. ~/.local/bin/myc -> ../share/…) so the real
    // binary is swapped, not the link.
    let exe = std::env::current_exe().context("cannot locate the running myc binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    match install_asset(&bytes, &expected, &exe)? {
        Staged::Replaced(path) => {
            println!("updated myc {current} → {latest} at {}", path.display());
            println!("`myc --version` should now report {latest}.");
            Ok(0)
        }
        Staged::StagedBeside(staged) => {
            // A running .exe cannot replace itself on Windows.
            println!("verified myc {latest} → {}", staged.display());
            println!();
            println!("A running .exe cannot replace itself on Windows. To finish:");
            println!(
                "  1. close every myc console / dashboard using {}",
                exe.display()
            );
            println!(
                "  2. swap the binaries (cmd):      move /Y \"{}\" \"{}\"",
                staged.display(),
                exe.display()
            );
            println!(
                "     or PowerShell:               Move-Item -Force \"{}\" \"{}\"",
                staged.display(),
                exe.display()
            );
            println!("  3. run `myc --version` — it should report {latest}.");
            println!(
                "note: the next myc command re-provisions the WSL twin from this \
                 new .exe's embedded copy — the twin follows automatically."
            );
            Ok(0)
        }
    }
}

// ----------------------------------------------------------------------
// Tests — no network anywhere: parsing and decisions are pure, the swap
// logic runs against local files.
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_with_or_without_v_prefix() {
        assert_eq!(
            Version::parse("1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(
            Version::parse("v1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(
            Version::parse(" v0.10.0\n"),
            Some(Version {
                major: 0,
                minor: 10,
                patch: 0
            })
        );
    }

    #[test]
    fn prerelease_and_build_suffixes_are_ignored() {
        assert_eq!(Version::parse("1.3.0-rc1"), Version::parse("1.3.0"));
        assert_eq!(Version::parse("v2.0.0-beta.2"), Version::parse("2.0.0"));
        assert_eq!(Version::parse("1.0.0+build.7"), Version::parse("1.0.0"));
    }

    #[test]
    fn non_version_tags_are_rejected() {
        for bad in [
            "latest",
            "nightly",
            "v1.2",
            "1.2",
            "1.2.x",
            "1.2.3.4",
            "",
            "v",
            "1.2.3.4.5",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn versions_compare_numerically_not_lexically() {
        // The classic lexical trap: "10" < "9" as strings.
        assert!(Version::parse("1.2.3").unwrap() < Version::parse("1.10.0").unwrap());
        assert!(Version::parse("0.3.0").unwrap() < Version::parse("0.3.1").unwrap());
        assert!(Version::parse("2.0.0").unwrap() > Version::parse("1.99.99").unwrap());
        assert_eq!(Version::parse("v1.2.3"), Version::parse("1.2.3"));
    }

    // Captured shape of GET /repos/Felmond13/mycel/releases/latest (asset
    // list trimmed to the interesting fields, URLs pointed at the fixture
    // server the real end-to-end test uses).
    const FIXTURE: &str = r#"{
  "url": "https://api.github.com/repos/Felmond13/mycel/releases/123",
  "tag_name": "v0.4.0",
  "draft": false,
  "prerelease": false,
  "assets": [
    {
      "name": "myc-linux-x86_64",
      "browser_download_url": "http://127.0.0.1:8971/myc-linux-x86_64"
    },
    {
      "name": "myc-linux-x86_64.sha256",
      "browser_download_url": "http://127.0.0.1:8971/myc-linux-x86_64.sha256"
    },
    {
      "name": "myc-windows-x86_64.exe",
      "browser_download_url": "http://127.0.0.1:8971/myc-windows-x86_64.exe"
    }
  ]
}"#;

    #[test]
    fn release_fixture_is_parsed() {
        let release = parse_release(FIXTURE).expect("fixture must parse");
        assert_eq!(release.tag, "v0.4.0");
        assert_eq!(
            release.version,
            Some(Version {
                major: 0,
                minor: 4,
                patch: 0
            })
        );
        assert_eq!(
            release.asset_url("myc-linux-x86_64"),
            Some("http://127.0.0.1:8971/myc-linux-x86_64")
        );
        assert_eq!(
            release.asset_url("myc-windows-x86_64.exe"),
            Some("http://127.0.0.1:8971/myc-windows-x86_64.exe")
        );
        assert_eq!(release.asset_url("myc-aarch64"), None);
    }

    #[test]
    fn non_version_tags_and_broken_bodies_are_rejected() {
        let unversioned = FIXTURE.replace("v0.4.0", "nightly");
        let release = parse_release(&unversioned).expect("body still parses");
        assert_eq!(release.version, None); // nothing to update to
        assert_eq!(parse_release("not json at all"), None);
        assert_eq!(parse_release("{\"assets\": []}"), None); // no tag_name
    }

    // sha256("hello"), sha256("") and sha256("new binary") — all
    // cross-checked with sha256sum(1).
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const NEW_BINARY_SHA: &str = "2f17c9ffb972a6c5da72c2b3df01f7e2ccf52dad2c0059dac631232a15126d2e";

    #[test]
    fn sidecars_parse_in_sha256sum_format() {
        assert_eq!(
            parse_sidecar(&format!("{HELLO_SHA}  myc-linux-x86_64\n")),
            Some(HELLO_SHA.to_string())
        );
        // Uppercase digests (some mirrors) are normalized to lowercase.
        assert_eq!(
            parse_sidecar(&format!("{}  myc-linux-x86_64\n", HELLO_SHA.to_uppercase())),
            Some(HELLO_SHA.to_string())
        );
    }

    #[test]
    fn malformed_sidecars_are_rejected() {
        assert_eq!(parse_sidecar(""), None);
        assert_eq!(parse_sidecar("mismatch\n"), None);
        assert_eq!(parse_sidecar("zzzz  myc-linux-x86_64\n"), None);
        // 63 hex chars: truncated digest.
        assert_eq!(parse_sidecar(&format!("{} x\n", &HELLO_SHA[..63])), None);
    }

    #[test]
    fn sha256_verification_accepts_the_real_digest() {
        assert!(verify_sha256(b"hello", HELLO_SHA).is_ok());
        assert!(verify_sha256(b"hello", &HELLO_SHA.to_uppercase()).is_ok());
        assert!(verify_sha256(b"", EMPTY_SHA).is_ok());
        let err = verify_sha256(b"tampered", HELLO_SHA)
            .unwrap_err()
            .to_string();
        assert!(err.contains("sha256 mismatch"), "{err}");
    }

    #[test]
    fn install_replaces_a_local_binary_and_keeps_it_executable() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("myc");
        std::fs::write(&exe, b"old binary").unwrap();
        install_asset(b"new binary", NEW_BINARY_SHA, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new binary");
        assert!(!dir.path().join("myc.new").exists(), "no .new leftover");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "the swapped binary stays executable");
        }
    }

    #[test]
    fn sha_mismatch_leaves_the_current_binary_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("myc");
        std::fs::write(&exe, b"old binary").unwrap();
        let err = install_asset(b"attacker bytes", HELLO_SHA, &exe)
            .unwrap_err()
            .to_string();
        assert!(err.contains("sha256 mismatch"), "{err}");
        assert_eq!(
            std::fs::read(&exe).unwrap(),
            b"old binary",
            "a failed verify must not touch the running binary"
        );
        assert!(
            !dir.path().join("myc.new").exists(),
            "no .new leftover after refusal"
        );
    }

    #[test]
    fn cache_round_trips_and_rejects_garbage() {
        let raw = cache_contents(1_700_000_000, "0.4.0");
        assert_eq!(
            parse_cache(&raw),
            Some((1_700_000_000, "0.4.0".to_string()))
        );
        assert_eq!(parse_cache(""), None);
        assert_eq!(parse_cache("{\"checked_at\": 12}"), None); // no "latest"
        assert_eq!(
            parse_cache("{\"checked_at\": \"x\", \"latest\": \"1.0.0\"}"),
            None
        );
    }

    #[test]
    fn cache_ttl_is_24_hours() {
        assert_eq!(CACHE_TTL_SECS, 24 * 3600);
        // Fresh at check time + 24h - 1s, stale one second later.
        let (at, now) = (1_700_000_000u64, 1_700_000_000 + 24 * 3600);
        assert!(now - at >= CACHE_TTL_SECS);
        assert!((now - 1) - at < CACHE_TTL_SECS);
    }

    #[test]
    fn doctor_line_matches_the_specified_wording() {
        let cur = "0.3.0";
        let next = Version::parse("0.4.0");
        assert_eq!(
            doctor_line(cur, next),
            "update available: 0.3.0 → 0.4.0 (myc update)"
        );
        assert_eq!(
            doctor_line(cur, Version::parse("0.3.0")),
            "up to date (0.3.0)"
        );
        assert_eq!(
            doctor_line(cur, Version::parse("0.2.9")),
            "up to date (0.3.0)"
        );
        assert_eq!(
            doctor_line(cur, None),
            "up to date (0.3.0) — could not check for updates (offline?)"
        );
        assert!(doctor_line("weird", next).contains("version unknown"));
    }

    #[test]
    fn windows_routing_sends_host_and_help_native_everything_else_to_the_twin() {
        use std::ffi::OsString;
        let args = |v: &[&str]| -> Vec<OsString> { v.iter().map(OsString::from).collect() };
        assert_eq!(route_windows_update(&args(&["update"])), UpdateRoute::Twin);
        assert_eq!(
            route_windows_update(&args(&["update", "--store"])),
            UpdateRoute::Twin
        );
        assert_eq!(
            route_windows_update(&args(&["update", "--host"])),
            UpdateRoute::Native
        );
        assert_eq!(
            route_windows_update(&args(&["update", "--help"])),
            UpdateRoute::Native
        );
        assert_eq!(
            route_windows_update(&args(&["update", "-h"])),
            UpdateRoute::Native
        );
    }

    #[test]
    fn asset_name_matches_the_release_workflow() {
        #[cfg(windows)]
        assert_eq!(ASSET_NAME, "myc-windows-x86_64.exe");
        #[cfg(not(windows))]
        assert_eq!(ASSET_NAME, "myc-linux-x86_64");
    }
}
