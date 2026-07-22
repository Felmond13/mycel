//! Native Linux backend: containers run in unprivileged user namespaces via
//! `myc-run`, exactly as `myc run` always has.

use crate::{BackendCheck, BackendReport, ExecBackend, Result, RunRequest};
use myc_store::Store;
use std::path::Path;

pub struct NativeLinux;

impl ExecBackend for NativeLinux {
    fn name(&self) -> &'static str {
        "native-linux"
    }

    fn probe(&self, store_root: &Path) -> BackendReport {
        let userns = match myc_run::probe_userns() {
            Ok(()) => BackendCheck {
                name: "user namespaces".into(),
                ok: true,
                detail: "unprivileged user namespaces work (probe passed)".into(),
                fix: None,
            },
            Err(e) => BackendCheck {
                name: "user namespaces".into(),
                ok: false,
                detail: e,
                fix: Some(
                    "enable them: `sudo sysctl -w kernel.unprivileged_userns_clone=1` \
                     (Debian/Ubuntu) or check your distro's user-namespace policy"
                        .into(),
                ),
            },
        };
        let store = match Store::open(store_root) {
            Ok(s) => BackendCheck {
                name: "content store".into(),
                ok: true,
                detail: format!("store at {} opens", s.root().display()),
                fix: None,
            },
            Err(e) => BackendCheck {
                name: "content store".into(),
                ok: false,
                detail: format!("cannot open store: {e}"),
                fix: Some("set MYCEL_STORE to a writable directory".into()),
            },
        };
        let available = userns.ok && store.ok;
        BackendReport {
            backend: self.name().to_string(),
            available,
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            checks: vec![userns, store],
        }
    }

    fn run(&self, store_root: &Path, request: &RunRequest) -> Result<i32> {
        let store = Store::open(store_root)?;
        let id = store.resolve(&request.reference)?;
        let manifest = store.get_manifest(&id)?;

        let cwd = std::env::current_dir()?;
        let mut binds = Vec::new();
        for spec in &request.binds {
            binds.push(crate::split_bind(spec, &cwd)?);
        }
        let (map_uid, map_gid) = match request.map_user.as_deref() {
            Some(spec) => {
                let (u, g) = spec.split_once(':').unwrap_or((spec, spec));
                (
                    u.parse().map_err(|_| {
                        crate::BackendError::Unavailable(format!("invalid map_user '{spec}'"))
                    })?,
                    g.parse().map_err(|_| {
                        crate::BackendError::Unavailable(format!("invalid map_user '{spec}'"))
                    })?,
                )
            }
            None => (0, 0),
        };
        let options = myc_run::RunOptions {
            command: request.command.clone(),
            env: request.env.clone(),
            hostname: if request.hostname.is_empty() {
                "mycel".to_string()
            } else {
                request.hostname.clone()
            },
            binds,
            workdir: request.workdir.clone(),
            keep_rootfs: request.keep_rootfs,
            // The desktop backend's `isolated_network` means "own
            // namespace, no connectivity" — Loopback in runtime terms.
            network: if request.isolated_network {
                myc_run::Network::Loopback
            } else {
                myc_run::Network::Host
            },
            map_uid,
            map_gid,
        };
        let run_dir = tempfile::Builder::new()
            .prefix("backend-run-")
            .tempdir_in(store.root().join("tmp"))
            .map_err(crate::BackendError::Io)?;
        let code = myc_run::run(&store, &manifest, run_dir.path(), &options)?;
        if options.keep_rootfs {
            let kept = run_dir.keep();
            eprintln!("rootfs kept at {}", kept.display());
        }
        Ok(code)
    }
}
