//! macOS VM backend — scaffold only, not yet available.
//!
//! The plan (see `docs/desktop-architecture.md`): a minimal pinned Linux
//! kernel + initramfs containing the Mycel runtime, booted through
//! Virtualization.framework, with the store shared over virtiofs. Until
//! that ships, every probe/run reports a clear "not yet available" with a
//! pointer to the architecture document. Nothing here is pretended to work.

use crate::{BackendCheck, BackendError, BackendReport, ExecBackend, Result, RunRequest};
use std::path::Path;

const NOT_YET: &str = "the macOS backend (lightweight Linux VM via Virtualization.framework) is \
                       not yet available — see docs/desktop-architecture.md for the plan; today, \
                       run myc on Linux or through Windows/WSL2";

pub struct Vm;

impl ExecBackend for Vm {
    fn name(&self) -> &'static str {
        "vm"
    }

    fn probe(&self, _store_root: &Path) -> BackendReport {
        BackendReport {
            backend: self.name().to_string(),
            available: false,
            version: None,
            checks: vec![BackendCheck {
                name: "vm backend".into(),
                ok: false,
                detail: NOT_YET.into(),
                fix: Some("track docs/desktop-architecture.md".into()),
            }],
        }
    }

    fn run(&self, _store_root: &Path, _request: &RunRequest) -> Result<i32> {
        Err(BackendError::Unavailable(NOT_YET.to_string()))
    }
}
