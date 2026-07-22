//! Environment health checks: can this machine actually run containers?
//!
//! Each check yields PASS/FAIL plus a one-line actionable fix, so a beginner
//! can go from a broken setup to a working one without reading kernel docs.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

fn check(name: &str, result: Result<String, (String, String)>) -> Check {
    match result {
        Ok(detail) => Check {
            name: name.to_string(),
            ok: true,
            detail,
            fix: None,
        },
        Err((detail, fix)) => Check {
            name: name.to_string(),
            ok: false,
            detail,
            fix: Some(fix),
        },
    }
}

fn check_linux() -> Result<String, (String, String)> {
    if !cfg!(target_os = "linux") {
        return Err((
            format!("running on {}, containers need Linux", std::env::consts::OS),
            "run myc inside WSL2 (`wsl --install`) or on a Linux machine".to_string(),
        ));
    }
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown kernel".to_string());
    let flavor = if release.to_lowercase().contains("microsoft") {
        " (WSL2)"
    } else {
        ""
    };
    Ok(format!("Linux {release}{flavor}"))
}

fn check_userns() -> Result<String, (String, String)> {
    match myc_run::probe_userns() {
        Ok(()) => Ok("unprivileged user namespaces work (probe passed)".to_string()),
        Err(e) => Err((
            e,
            "enable them: `sudo sysctl -w kernel.unprivileged_userns_clone=1` \
             (Debian/Ubuntu) or check your distro's user-namespace policy"
                .to_string(),
        )),
    }
}

fn check_store(store_root: &Path) -> Result<String, (String, String)> {
    let store = myc_store::Store::open(store_root).map_err(|e| {
        (
            format!("cannot open store at {}: {e}", store_root.display()),
            format!(
                "check permissions on {} or set MYCEL_STORE to a writable directory",
                store_root.display()
            ),
        )
    })?;
    let probe = store.root().join("tmp").join(".doctor-probe");
    std::fs::write(&probe, b"ok").map_err(|e| {
        (
            format!("store is not writable: {e}"),
            format!("fix permissions: `chmod -R u+w {}`", store_root.display()),
        )
    })?;
    let _ = std::fs::remove_file(&probe);
    Ok(format!("store at {} is writable", store.root().display()))
}

fn check_registry() -> Result<String, (String, String)> {
    match myc_oci::registry_reachable() {
        Ok(()) => Ok("Docker Hub (registry-1.docker.io) reachable over HTTPS".to_string()),
        Err(e) => Err((
            format!("cannot reach Docker Hub: {e}"),
            "check your internet connection / proxy; offline use still works \
             for already-ingested environments"
                .to_string(),
        )),
    }
}

/// Run every health check. Must be called from a plain (non-async) thread —
/// the registry check uses a blocking HTTP client.
pub fn run_checks(store_root: &Path) -> Vec<Check> {
    vec![
        check("operating system", check_linux()),
        check("user namespaces", check_userns()),
        check("content store", check_store(store_root)),
        check("registry network", check_registry()),
    ]
}
