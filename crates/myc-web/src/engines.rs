//! `/api/engines` — what this machine can execute containers with.
//!
//! Built on `myc_backend::detect_engines` (the same list `myc doctor`
//! prints), enriched with the detailed backend probe whenever the engine
//! has a probeable backend on this platform, so the UI can explain *why*
//! an engine is off (missing /dev/kvm, missing cloud-hypervisor…) instead
//! of just showing a boolean.

use crate::{ApiResult, App};
use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

/// The `/api/engines` payload: the machine's default engine plus every
/// isolation tier it could use, strongest first (detect order), each with
/// its degraded-feature list and — when the platform exposes a probeable
/// backend — the full check-by-check probe report.
pub fn engines_payload(store_root: &std::path::Path) -> Value {
    let detected = myc_backend::detect_engines(store_root);
    // The default is the strongest *complete* engine — what `myc run`
    // (auto) and `myc doctor` pick for a simple request; per-request
    // fallbacks (pods, --run-dir, rootless port publishing…) are resolved
    // at start time by the same policy (myc_backend::decide_auto_engine).
    // Never absent: fall back to native.
    let default = detected
        .iter()
        .find(|e| e.available && e.complete)
        .map(|e| e.kind.label())
        .unwrap_or("native-namespaces");

    let engines: Vec<Value> = detected
        .iter()
        .map(|e| {
            let mut entry = json!({
                "kind": e.kind.label(),
                "label": e.kind.label(),
                "available": e.available,
                "complete": e.complete,
                "degraded": e.kind.degraded_features(),
            });
            if let Some(probe) = probe_for(e.kind, store_root) {
                entry
                    .as_object_mut()
                    .expect("engine entry is an object")
                    .insert("probe".into(), probe);
            }
            entry
        })
        .collect();

    json!({ "default": default, "engines": engines })
}

/// Detailed probe report for the engines that have a backend on this
/// platform (`None` for tiers that are pure plans here, e.g. the macOS
/// Hypervisor.framework tier listed on Linux).
fn probe_for(kind: myc_backend::EngineKind, store_root: &std::path::Path) -> Option<Value> {
    #[cfg(target_os = "linux")]
    {
        use myc_backend::ExecBackend;
        let backend: Box<dyn ExecBackend> = match kind {
            myc_backend::EngineKind::KvmMicroVm => {
                Box::new(myc_backend::kvm::KvmMicroVm::from_env())
            }
            myc_backend::EngineKind::NativeNamespaces => Box::new(myc_backend::native::NativeLinux),
            _ => return None,
        };
        let report = backend.probe(store_root);
        Some(json!({
            "backend": report.backend,
            "available": report.available,
            "version": report.version,
            "checks": report.checks.iter().map(|c| json!({
                "name": c.name,
                "ok": c.ok,
                "detail": c.detail,
                "fix": c.fix,
            })).collect::<Vec<_>>(),
        }))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (kind, store_root);
        None
    }
}

/// `GET /api/engines`.
pub async fn list(State(app): State<App>) -> ApiResult {
    crate::blocking(move || Ok(Json(engines_payload(&app.root)))).await
}

#[cfg(test)]
mod tests {
    use super::engines_payload;
    use serde_json::Value;

    /// The contract the frontend codes against: a non-empty `default`
    /// label and an `engines` array whose entries all carry the five
    /// documented fields, `degraded` being a list of strings.
    #[test]
    fn engines_payload_matches_the_api_contract() {
        let dir = tempfile::tempdir().unwrap();
        let payload = engines_payload(dir.path());
        let default = payload["default"].as_str().expect("default is a string");
        assert!(!default.is_empty());
        let engines = payload["engines"].as_array().expect("engines is an array");
        assert!(!engines.is_empty(), "at least one engine is listed");
        for e in engines {
            for key in ["kind", "label", "available", "complete", "degraded"] {
                assert!(e.get(key).is_some(), "engine missing '{key}': {e}");
            }
            assert_eq!(e["kind"], e["label"]);
            assert!(
                e["degraded"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(Value::is_string),
                "degraded must be a list of strings: {}",
                e["degraded"]
            );
        }
    }
}
