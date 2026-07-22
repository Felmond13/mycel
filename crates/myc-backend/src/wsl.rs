//! WSL2 backend: `myc.exe` as a transparent front for a Linux twin.
//!
//! Containers are Linux processes, so on Windows every command that touches
//! the store or runs containers is proxied to a "twin" Linux `myc` binary
//! living inside WSL2 at `~/.mycel-bin/myc`. There is exactly ONE store —
//! the twin's `~/.mycel` inside WSL — which keeps hardlink materialization
//! fast (native ext4) and makes every feature (ingest, lazy streaming, hub,
//! builds, the web UI) behave identically to Linux. `myc.exe ui` proxies
//! too: the server runs inside WSL and WSL2's localhost forwarding makes it
//! reachable from Windows browsers.
//!
//! Twin provisioning, in priority order:
//! 1. `$MYCEL_WSL_BINARY` — path to a Linux `myc` build; a WSL path
//!    (`/home/...`) is copied inside WSL, a Windows path is streamed in.
//! 2. The Linux binary embedded in `myc.exe` at build time (release
//!    packaging sets `MYCEL_LINUX_BINARY`; ~9 MB, makes the .exe fully
//!    self-contained).
//! 3. An already-provisioned twin is used as-is (dev builds without an
//!    embedded twin).
//!
//! Provisioning is idempotent and self-healing: the twin's sha256 is
//! compared to the desired bytes on every command, so upgrading `myc.exe`
//! upgrades the twin on its next run.
//!
//! Distro selection: `$MYCEL_WSL_DISTRO`, else the default (first) entry of
//! `wsl.exe --list --quiet`.

use crate::{BackendCheck, BackendError, BackendReport, ExecBackend, Result, RunRequest};
use sha2::Digest;
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// The Linux twin embedded at build time (empty when `MYCEL_LINUX_BINARY`
/// was not set — e.g. plain dev builds).
static EMBEDDED_TWIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/myc-linux-twin"));

/// Executes the twin with full argument fidelity and no extra shell parsing:
/// `wsl.exe -d <distro> --exec sh -c '<TWIN_EXEC>' myc <args…>` — `"$@"`
/// forwards every argument verbatim, spaces and all.
const TWIN_EXEC: &str = r#"exec "$HOME/.mycel-bin/myc" "$@""#;

pub struct Wsl2 {
    distro_override: Option<String>,
    distro: OnceLock<std::result::Result<String, String>>,
}

impl Wsl2 {
    pub fn from_env() -> Self {
        Wsl2 {
            distro_override: std::env::var("MYCEL_WSL_DISTRO")
                .ok()
                .filter(|d| !d.is_empty()),
            distro: OnceLock::new(),
        }
    }

    /// The distro every proxied command targets (cached per process).
    pub fn distro(&self) -> Result<String> {
        self.distro
            .get_or_init(|| {
                if let Some(d) = &self.distro_override {
                    return Ok(d.clone());
                }
                match list_distros() {
                    ds if !ds.is_empty() => Ok(ds[0].clone()),
                    _ => Err("no WSL distribution found".to_string()),
                }
            })
            .clone()
            .map_err(|e| {
                BackendError::Unavailable(format!(
                    "{e} — install one with `wsl --install -d Ubuntu`, \
                     or point MYCEL_WSL_DISTRO at an existing distro"
                ))
            })
    }

    /// A `wsl.exe` command running `sh -c <script>` in the target distro.
    fn sh(&self, distro: &str, script: &str) -> Command {
        let mut cmd = Command::new("wsl.exe");
        cmd.arg("-d")
            .arg(distro)
            .arg("--exec")
            .arg("sh")
            .arg("-c")
            .arg(script);
        cmd
    }

    /// Capture stdout of `sh -c <script>` inside the distro.
    fn sh_output(&self, distro: &str, script: &str) -> Result<String> {
        let out = self
            .sh(distro, script)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| BackendError::Unavailable(format!("cannot invoke wsl.exe: {e}")))?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// sha256 of the currently provisioned twin, or empty when absent.
    fn twin_hash(&self, distro: &str) -> Result<String> {
        self.sh_output(
            distro,
            r#"sha256sum "$HOME/.mycel-bin/myc" 2>/dev/null | cut -d" " -f1"#,
        )
    }

    /// `myc X.Y.Z` from the provisioned twin, or None when absent/broken.
    pub fn twin_version(&self, distro: &str) -> Option<String> {
        self.sh_output(distro, r#""$HOME/.mycel-bin/myc" --version 2>/dev/null"#)
            .ok()
            .filter(|v| !v.is_empty())
    }

    /// Stream `bytes` into `~/.mycel-bin/myc` inside the distro (atomic:
    /// temp file + rename, executable bit set).
    fn install_twin_bytes(&self, distro: &str, bytes: &[u8]) -> Result<()> {
        let script = r#"mkdir -p "$HOME/.mycel-bin" && cat > "$HOME/.mycel-bin/.myc.tmp" && chmod +x "$HOME/.mycel-bin/.myc.tmp" && mv "$HOME/.mycel-bin/.myc.tmp" "$HOME/.mycel-bin/myc""#;
        let mut child = self
            .sh(distro, script)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| BackendError::Provision(format!("cannot invoke wsl.exe: {e}")))?;
        child
            .stdin
            .take()
            .expect("stdin piped")
            .write_all(bytes)
            .map_err(|e| BackendError::Provision(format!("writing twin into WSL failed: {e}")))?;
        let status = child
            .wait()
            .map_err(|e| BackendError::Provision(format!("wsl.exe failed: {e}")))?;
        if !status.success() {
            return Err(BackendError::Provision(
                "installing the Linux twin inside WSL failed".to_string(),
            ));
        }
        Ok(())
    }

    /// Make sure `~/.mycel-bin/myc` inside the distro matches the desired
    /// twin, provisioning or upgrading it when needed. Returns a short
    /// human description of what the twin is.
    pub fn ensure_twin(&self, distro: &str) -> Result<String> {
        // 1. Explicit override: $MYCEL_WSL_BINARY.
        if let Some(src) = std::env::var("MYCEL_WSL_BINARY")
            .ok()
            .filter(|s| !s.is_empty())
        {
            if src.starts_with('/') {
                // A path inside WSL: compare hashes there, cp when stale.
                let src_hash = self.sh_output(
                    distro,
                    &format!(r#"sha256sum "{src}" 2>/dev/null | cut -d" " -f1"#),
                )?;
                if src_hash.is_empty() {
                    return Err(BackendError::Provision(format!(
                        "MYCEL_WSL_BINARY={src} not found inside WSL distro '{distro}'"
                    )));
                }
                if self.twin_hash(distro)? != src_hash {
                    let script = format!(
                        r#"mkdir -p "$HOME/.mycel-bin" && cp "{src}" "$HOME/.mycel-bin/.myc.tmp" && chmod +x "$HOME/.mycel-bin/.myc.tmp" && mv "$HOME/.mycel-bin/.myc.tmp" "$HOME/.mycel-bin/myc""#
                    );
                    let status = self.sh(distro, &script).status().map_err(|e| {
                        BackendError::Provision(format!("cannot invoke wsl.exe: {e}"))
                    })?;
                    if !status.success() {
                        return Err(BackendError::Provision(format!(
                            "copying {src} to ~/.mycel-bin/myc inside WSL failed"
                        )));
                    }
                }
                return Ok(format!("provisioned from $MYCEL_WSL_BINARY ({src})"));
            }
            // A Windows path: read the bytes here, stream them in.
            let bytes = std::fs::read(&src).map_err(|e| {
                BackendError::Provision(format!("cannot read MYCEL_WSL_BINARY={src}: {e}"))
            })?;
            if self.twin_hash(distro)? != sha256_hex(&bytes) {
                self.install_twin_bytes(distro, &bytes)?;
            }
            return Ok(format!("provisioned from $MYCEL_WSL_BINARY ({src})"));
        }

        // 2. The twin embedded in this .exe.
        if !EMBEDDED_TWIN.is_empty() {
            if self.twin_hash(distro)? != sha256_hex(EMBEDDED_TWIN) {
                eprintln!(
                    "myc: provisioning Linux twin into WSL distro '{distro}' (~/.mycel-bin/myc, {:.1} MB)…",
                    EMBEDDED_TWIN.len() as f64 / 1e6
                );
                self.install_twin_bytes(distro, EMBEDDED_TWIN)?;
            }
            return Ok("embedded in myc.exe (self-contained)".to_string());
        }

        // 3. No source: an existing twin is fine, nothing at all is not.
        if self.twin_version(distro).is_some() {
            return Ok(
                "pre-existing ~/.mycel-bin/myc (no embedded twin in this build)".to_string(),
            );
        }
        Err(BackendError::Provision(format!(
            "no Linux twin available in WSL distro '{distro}': this myc.exe has no embedded \
             twin — set MYCEL_WSL_BINARY to a Linux `myc` build, or use a release myc.exe"
        )))
    }

    /// Proxy a full `myc` command line to the twin with inherited stdio.
    /// This is the default path for (almost) every command on Windows.
    pub fn proxy(&self, args: &[OsString]) -> Result<i32> {
        let distro = self.distro()?;
        self.ensure_twin(&distro)?;
        let mut cmd = Command::new("wsl.exe");
        cmd.arg("-d")
            .arg(&distro)
            .arg("--exec")
            .arg("sh")
            .arg("-c")
            .arg(TWIN_EXEC)
            .arg("myc") // $0 for the sh snippet
            .args(args);
        forward_hub_env(&mut cmd);
        let status = cmd
            .status()
            .map_err(|e| BackendError::Unavailable(format!("cannot invoke wsl.exe: {e}")))?;
        Ok(status.code().unwrap_or(1))
    }
}

impl ExecBackend for Wsl2 {
    fn name(&self) -> &'static str {
        "wsl2"
    }

    fn probe(&self, _store_root: &Path) -> BackendReport {
        let mut checks = Vec::new();

        // WSL present?
        let distros = list_distros();
        checks.push(if distros.is_empty() {
            BackendCheck {
                name: "wsl2".into(),
                ok: false,
                detail: "WSL is not installed or has no distribution".into(),
                fix: Some("install it: `wsl --install -d Ubuntu` (then reboot)".into()),
            }
        } else {
            BackendCheck {
                name: "wsl2".into(),
                ok: true,
                detail: format!(
                    "WSL detected ({} distro(s): {})",
                    distros.len(),
                    distros.join(", ")
                ),
                fix: None,
            }
        });

        // Distro selection.
        let distro = match self.distro() {
            Ok(d) => {
                checks.push(BackendCheck {
                    name: "distro".into(),
                    ok: true,
                    detail: format!(
                        "using '{d}'{}",
                        if self.distro_override.is_some() {
                            " (from $MYCEL_WSL_DISTRO)"
                        } else {
                            " (WSL default; override with $MYCEL_WSL_DISTRO)"
                        }
                    ),
                    fix: None,
                });
                Some(d)
            }
            Err(e) => {
                checks.push(BackendCheck {
                    name: "distro".into(),
                    ok: false,
                    detail: e.to_string(),
                    fix: Some("`wsl --install -d Ubuntu`".into()),
                });
                None
            }
        };

        // Twin provisioning (provisions on the spot when missing/stale).
        let mut version = None;
        if let Some(d) = &distro {
            match self.ensure_twin(d) {
                Ok(how) => {
                    version = self.twin_version(d);
                    checks.push(BackendCheck {
                        name: "linux twin".into(),
                        ok: true,
                        detail: format!(
                            "~/.mycel-bin/myc in '{d}' ok ({}) — {how}",
                            version.as_deref().unwrap_or("version unknown")
                        ),
                        fix: None,
                    });
                    let ours = format!("myc {}", env!("CARGO_PKG_VERSION"));
                    let matches = version.as_deref() == Some(ours.as_str());
                    checks.push(BackendCheck {
                        name: "version match".into(),
                        ok: matches,
                        detail: if matches {
                            format!("myc.exe and twin both {ours}")
                        } else {
                            format!(
                                "myc.exe is {ours}, twin is {}",
                                version.as_deref().unwrap_or("unknown")
                            )
                        },
                        fix: (!matches).then(|| {
                            "re-run any myc command with a release myc.exe (self-heals), or set \
                             MYCEL_WSL_BINARY to a matching Linux build"
                                .into()
                        }),
                    });
                }
                Err(e) => checks.push(BackendCheck {
                    name: "linux twin".into(),
                    ok: false,
                    detail: e.to_string(),
                    fix: Some(
                        "set MYCEL_WSL_BINARY to a Linux `myc` build, or use a release myc.exe \
                         with an embedded twin"
                            .into(),
                    ),
                }),
            }
        }

        BackendReport {
            backend: self.name().to_string(),
            available: checks.iter().all(|c| c.ok),
            version,
            checks,
        }
    }

    fn run(&self, _store_root: &Path, request: &RunRequest) -> Result<i32> {
        let mut args: Vec<OsString> = vec!["run".into()];
        for kv in &request.env {
            args.push("--env".into());
            args.push(kv.into());
        }
        for bind in &request.binds {
            args.push("--volume".into());
            args.push(bind.into());
        }
        if !request.hostname.is_empty() {
            args.push("--hostname".into());
            args.push(request.hostname.as_str().into());
        }
        if let Some(wd) = &request.workdir {
            args.push("--workdir".into());
            args.push(wd.into());
        }
        if let Some(map) = &request.map_user {
            args.push("--map-user".into());
            args.push(map.into());
        }
        if request.isolated_network {
            args.push("--isolated-network".into());
        }
        if request.keep_rootfs {
            args.push("--keep-rootfs".into());
        }
        args.push(request.reference.as_str().into());
        if !request.command.is_empty() {
            args.push("--".into());
            for c in &request.command {
                args.push(c.into());
            }
        }
        self.proxy(&args)
    }
}

/// Forward hub-related environment variables into WSL via WSLENV, so
/// `myc.exe push --to …` and `$MYCEL_HUB` behave as expected inside the twin.
fn forward_hub_env(cmd: &mut Command) {
    let mut wslenv: Vec<String> = std::env::var("WSLENV")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| v.split(':').map(str::to_string).collect())
        .unwrap_or_default();
    for var in ["MYCEL_HUB", "MYCEL_HUB_TOKEN"] {
        if std::env::var_os(var).is_some() && !wslenv.iter().any(|e| e == var) {
            wslenv.push(var.to_string());
        }
    }
    if !wslenv.is_empty() {
        cmd.env("WSLENV", wslenv.join(":"));
    }
}

/// Installed WSL distributions, default first (`wsl.exe --list --quiet`).
/// wsl.exe prints UTF-16LE; tolerate UTF-8 too.
fn list_distros() -> Vec<String> {
    let Ok(out) = Command::new("wsl.exe")
        .args(["--list", "--quiet"])
        .stdin(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    decode_console(&out.stdout)
        .lines()
        .map(|l| l.trim().trim_matches('\0').to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Console output from wsl.exe itself is UTF-16LE; output from Linux
/// programs is UTF-8. Sniff by NUL density.
fn decode_console(bytes: &[u8]) -> String {
    let nuls = bytes.iter().take(64).filter(|&&b| b == 0).count();
    if nuls > 4 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}
