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
                    "{e} — run `myc setup-windows` to install WSL2 + Ubuntu (guided), \
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
                fix: Some(
                    "run `myc setup-windows` (installs WSL2 + Ubuntu for you), or manually \
                     `wsl --install -d Ubuntu` (then reboot)"
                        .into(),
                ),
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
                    fix: Some("`myc setup-windows` (guided WSL2 + Ubuntu install)".into()),
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
        for (host, container) in &request.publish {
            args.push("--publish".into());
            args.push(format!("{host}:{container}").into());
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
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Guided first-run setup: `myc setup-windows`
//
// "Docker Desktop installed WSL2 for me — so should myc." The flow detects
// where the machine stands (WSL absent / WSL without a distro / WSL ready),
// offers to run the elevated `wsl --install -d Ubuntu` through a UAC prompt
// (PowerShell `Start-Process -Verb RunAs`), explains the required REBOOT,
// and persists a small marker under the store root so the first run after
// the reboot resumes at the right step: verify the twin end to end, then
// point at the dashboard. Declining the guided path prints step-by-step
// instructions instead. The detection and state-transition logic is pure
// text-in/text-out so it is unit-tested on Linux (this module compiles for
// `cfg(any(windows, test))`).
// ---------------------------------------------------------------------------

/// Microsoft's official WSL install guide (printed on the manual path).
pub const WSL_INSTALL_DOC_URL: &str = "https://learn.microsoft.com/windows/wsl/install";

/// Marker file under the host-side store root recording where the guided
/// setup stands across reboots (~/.mycel/setup-windows.state).
pub const SETUP_STATE_FILE: &str = "setup-windows.state";

/// What `myc setup-windows` found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WslStatus {
    /// wsl.exe is missing, or it exists as the inbox stub but the WSL
    /// optional component is disabled (it answers non-zero until enabled).
    /// Either way `wsl --install` is the remedy.
    Absent,
    /// WSL answers but no distribution is installed.
    NoDistro,
    /// WSL answers and has distributions (names, default first).
    Ready(Vec<String>),
}

/// Classify a machine from raw probe inputs (pure; the interactive flow
/// feeds it real wsl.exe output, unit tests feed captured bytes).
///
/// Existing distros win over a failing `--status`: on Windows builds where
/// `--status` is unsupported, a working distro still proves WSL is there.
pub fn classify_wsl(wsl_exe_found: bool, wsl_status_ok: bool, list_output: &str) -> WslStatus {
    if !wsl_exe_found {
        return WslStatus::Absent;
    }
    let distros = distros_from_list_output(list_output);
    if !distros.is_empty() {
        return WslStatus::Ready(distros);
    }
    if wsl_status_ok {
        WslStatus::NoDistro
    } else {
        // wsl.exe exists, refuses to answer and has no distro: the optional
        // component is off. Same fix as "not installed at all".
        WslStatus::Absent
    }
}

/// Parse decoded `wsl.exe --list --quiet` output into distro names (pure).
/// Blank lines, NUL padding and the human "no installed distributions"
/// sentence some Windows builds print instead of an empty list are ignored.
pub fn distros_from_list_output(list_output: &str) -> Vec<String> {
    const NO_DISTRO_HINTS: [&str; 2] = [
        "no installed distributions",
        // "Use 'wsl --list --online' … to install a distribution."
        "wsl --list --online",
    ];
    list_output
        .lines()
        .map(|l| l.trim().trim_matches('\0').trim())
        .filter(|l| !l.is_empty())
        .filter(|l| {
            let lower = l.to_lowercase();
            !NO_DISTRO_HINTS.iter().any(|h| lower.contains(h))
        })
        .map(str::to_string)
        .collect()
}

/// Probe this machine: is wsl.exe there, does it answer, any distro?
fn detect_wsl_status() -> WslStatus {
    let status = Command::new("wsl.exe")
        .arg("--status")
        .stdin(Stdio::null())
        .output();
    let status_ok = status.as_ref().is_ok_and(|o| o.status.success());
    let list_output = Command::new("wsl.exe")
        .args(["--list", "--quiet"])
        .stdin(Stdio::null())
        .output()
        .map(|o| decode_console(&o.stdout))
        .unwrap_or_default();
    classify_wsl(status.is_ok(), status_ok, &list_output)
}

/// Persisted stages of the guided setup (one keyword in the marker file) so
/// the flow survives the reboot `wsl --install` demands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStage {
    /// The elevated `wsl --install -d Ubuntu` was launched; the required
    /// reboot (and one first Ubuntu launch) is still pending.
    InstallStarted,
    /// WSL is ready and the Linux twin was verified — nothing left to do.
    Complete,
}

/// Parse the marker file (pure). Unknown/garbled content → None: the flow
/// simply re-detects from scratch, a stale marker is harmless.
pub fn parse_setup_stage(raw: &str) -> Option<SetupStage> {
    match raw.lines().next()?.trim() {
        "install-started" => Some(SetupStage::InstallStarted),
        "complete" => Some(SetupStage::Complete),
        _ => None,
    }
}

/// The keyword persisted for `stage` (pure).
pub fn setup_stage_name(stage: SetupStage) -> &'static str {
    match stage {
        SetupStage::InstallStarted => "install-started",
        SetupStage::Complete => "complete",
    }
}

/// The stage to persist after a run that observed `status` (pure):
///
/// - a ready WSL always completes the flow (the twin is verified before
///   persisting);
/// - a just-launched install marks "reboot pending";
/// - otherwise the marker is left as-is: still waiting for the reboot, or
///   the user declined and nothing ever started (None).
pub fn stage_to_persist(
    persisted: Option<SetupStage>,
    status: &WslStatus,
    launched_install: bool,
) -> Option<SetupStage> {
    if matches!(status, WslStatus::Ready(_)) {
        return Some(SetupStage::Complete);
    }
    if launched_install {
        return Some(SetupStage::InstallStarted);
    }
    persisted
}

/// True when the marker says an install was launched but WSL still is not
/// ready — the reboot / first Ubuntu launch is still pending (pure).
pub fn reboot_pending(persisted: Option<SetupStage>, status: &WslStatus) -> bool {
    persisted == Some(SetupStage::InstallStarted) && !matches!(status, WslStatus::Ready(_))
}

/// Write the marker (best effort: losing it only loses reboot resumption).
fn persist_stage(path: &Path, stage: SetupStage) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{}\n", setup_stage_name(stage)));
}

/// Ask a yes/no question on an interactive stdin. Default is yes; anything
/// starting with n/N (or EOF, or a non-terminal stdin) declines — we never
/// elevate without an explicit human answer.
fn confirm(question: &str) -> bool {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return false;
    }
    println!("{question} [Y/n]");
    let mut answer = String::new();
    // Ok(0) = EOF: treat silence as a refusal, not as the default yes.
    let answered = matches!(std::io::stdin().read_line(&mut answer), Ok(n) if n > 0);
    answered && !answer.trim().to_lowercase().starts_with('n')
}

/// Elevate through a UAC prompt and run the installer to completion:
/// `Start-Process -Verb RunAs wsl.exe --install -d Ubuntu` (single-quoted
/// PowerShell strings — nothing to interpolate, no bare `$` or paths).
/// Returns Ok(true) when the elevated process exited 0.
fn launch_elevated_wsl_install() -> std::result::Result<bool, String> {
    let script = "Start-Process -Verb RunAs -FilePath wsl.exe \
                  -ArgumentList '--install','-d','Ubuntu' -Wait";
    match Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", script])
        .stdin(Stdio::null())
        .status()
    {
        Ok(status) => Ok(status.success()),
        Err(e) => Err(format!("cannot invoke powershell.exe: {e}")),
    }
}

/// Step-by-step instructions for the do-it-myself path.
fn print_manual_instructions() {
    println!();
    println!("Step-by-step (no automation):");
    println!("  1. Open PowerShell or cmd as Administrator (Start menu, right-click).");
    println!("  2. Run: wsl --install -d Ubuntu");
    println!("  3. RESTART your computer when asked — this is required, not optional.");
    println!("  4. After the reboot, launch \"Ubuntu\" once from the Start menu and");
    println!("     create your Linux username/password if prompted.");
    println!("  5. Run `myc setup-windows` again — it verifies the whole setup end to end.");
    println!();
    println!("Official guide: {WSL_INSTALL_DOC_URL}");
}

/// End-to-end verification: pick the distro, provision the twin, confirm
/// the version matches this myc.exe.
fn verify_twin() -> Result<()> {
    let wsl = Wsl2::from_env();
    let distro = wsl.distro()?;
    let how = wsl.ensure_twin(&distro)?;
    let version = wsl.twin_version(&distro);
    println!(
        "Linux twin in '{distro}' ok ({}) — {how}",
        version.as_deref().unwrap_or("version unknown")
    );
    let ours = format!("myc {}", env!("CARGO_PKG_VERSION"));
    if version.as_deref() != Some(ours.as_str()) {
        eprintln!(
            "myc: warning: myc.exe is {ours}, twin is {} — any proxied command self-heals this",
            version.as_deref().unwrap_or("unknown")
        );
    }
    Ok(())
}

/// `myc setup-windows`: the guided first-run flow (host-side, never
/// proxied). Returns a process exit code.
pub fn setup_windows() -> i32 {
    let state_path = myc_store::Store::default_root().join(SETUP_STATE_FILE);
    let persisted = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|raw| parse_setup_stage(&raw));
    let status = detect_wsl_status();

    match &status {
        WslStatus::Ready(distros) => {
            println!(
                "WSL2 is ready ({} distro(s): {}).",
                distros.len(),
                distros.join(", ")
            );
            if persisted == Some(SetupStage::InstallStarted) {
                println!("The install you launched earlier finished — completing the setup…");
            }
            match verify_twin() {
                Ok(()) => {
                    // stage_to_persist(_, ready, _) == Complete.
                    persist_stage(&state_path, SetupStage::Complete);
                    println!();
                    println!("All set. Try:");
                    println!("  myc shell alpine:3.20   interactive shell in any Docker Hub image");
                    println!("  myc ui                  the dashboard (http://localhost:7777)");
                    0
                }
                Err(e) => {
                    eprintln!("myc: error: {e:#}");
                    eprintln!(
                        "myc: hint: fix the above, then re-run `myc setup-windows`; `myc doctor` \
                         prints the full Windows + WSL2 picture"
                    );
                    1
                }
            }
        }
        WslStatus::Absent | WslStatus::NoDistro => {
            // An install launched on a previous run: guide the reboot and
            // the first Ubuntu launch instead of offering to install again.
            if reboot_pending(persisted, &status) {
                println!("The WSL2 + Ubuntu install you launched earlier is waiting on you:");
                println!("  1. RESTART your computer — the install enables Windows features");
                println!("     that only take effect after a reboot.");
                println!("  2. After the reboot, launch \"Ubuntu\" once from the Start menu and");
                println!("     create your Linux username/password if prompted.");
                println!();
                println!("Then run `myc setup-windows` again (any other myc command also picks");
                println!("the setup back up automatically). If the install never completed,");
                println!("relaunch it from an administrator terminal: wsl --install -d Ubuntu");
                println!();
                println!("Official guide: {WSL_INSTALL_DOC_URL}");
                return 0;
            }

            let missing = match status {
                WslStatus::Absent => "this machine has no usable WSL2",
                WslStatus::NoDistro => "WSL2 is installed but has no Linux distribution",
                WslStatus::Ready(_) => unreachable!("handled above"),
            };
            println!("myc runs containers inside WSL2 — the Linux layer Docker Desktop");
            println!("used to install for you. Right now, {missing}.");

            if confirm("Install WSL2 + Ubuntu for you now?") {
                // Marker BEFORE the elevation: if the machine reboots
                // mid-install, the next run guides the reboot instead of
                // re-offering (stage_to_persist(_, _, true)).
                persist_stage(&state_path, SetupStage::InstallStarted);
                println!();
                println!("Launching the installer — accept the Windows UAC prompt…");
                match launch_elevated_wsl_install() {
                    Ok(true) => {
                        println!();
                        println!("WSL2 + Ubuntu install launched.");
                        println!("A RESTART of your computer is REQUIRED to finish it.");
                        println!("After the reboot, launch \"Ubuntu\" once from the Start menu,");
                        println!(
                            "then run `myc setup-windows` again — it verifies the whole setup"
                        );
                        println!("end to end (any other myc command resumes the setup too).");
                        0
                    }
                    Ok(false) => {
                        eprintln!(
                            "myc: the elevated installer did not complete (UAC declined or \
                             wsl --install failed)."
                        );
                        print_manual_instructions();
                        1
                    }
                    Err(e) => {
                        eprintln!("myc: error: {e}");
                        print_manual_instructions();
                        1
                    }
                }
            } else {
                // Declined (or non-interactive): nothing is changed, and no
                // marker is written (stage_to_persist(_, _, false) → None).
                print_manual_instructions();
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distro_lists_are_parsed_from_utf16_and_utf8_output() {
        // wsl.exe itself prints UTF-16LE with NUL padding.
        let utf16: Vec<u8> = "Ubuntu\r\nUbuntu-22.04\r\n\0\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let decoded = decode_console(&utf16);
        assert_eq!(
            distros_from_list_output(&decoded),
            ["Ubuntu", "Ubuntu-22.04"]
        );
        // Output relayed from Linux-side programs is plain UTF-8.
        assert_eq!(
            distros_from_list_output("Ubuntu\nDebian\n"),
            ["Ubuntu", "Debian"]
        );
    }

    #[test]
    fn no_distro_messages_are_not_distro_names() {
        assert!(distros_from_list_output("").is_empty());
        assert!(distros_from_list_output("\0\0\0\0").is_empty());
        assert!(distros_from_list_output("  \r\n \n").is_empty());
        // Some Windows builds print a sentence instead of an empty list.
        assert!(distros_from_list_output(
            "Windows Subsystem for Linux has no installed distributions.\r\n"
        )
        .is_empty());
        assert!(distros_from_list_output("Use 'wsl --list --online' to install one.\n").is_empty());
    }

    #[test]
    fn classification_covers_the_three_states() {
        // No wsl.exe at all.
        assert_eq!(classify_wsl(false, false, ""), WslStatus::Absent);
        assert_eq!(classify_wsl(false, true, "Ubuntu"), WslStatus::Absent);
        // WSL answers, no distro installed.
        assert_eq!(classify_wsl(true, true, ""), WslStatus::NoDistro);
        // wsl.exe exists but the optional component is off (stub answers
        // non-zero): same remedy as absent.
        assert_eq!(classify_wsl(true, false, ""), WslStatus::Absent);
        assert_eq!(
            classify_wsl(true, false, "has no installed distributions"),
            WslStatus::Absent
        );
        // Existing distros win over a failing --status (old builds).
        assert_eq!(
            classify_wsl(true, false, "Ubuntu"),
            WslStatus::Ready(vec!["Ubuntu".to_string()])
        );
        assert_eq!(
            classify_wsl(true, true, "Ubuntu\nDebian"),
            WslStatus::Ready(vec!["Ubuntu".to_string(), "Debian".to_string()])
        );
    }

    #[test]
    fn setup_stage_marker_round_trips() {
        assert_eq!(
            parse_setup_stage("install-started\n"),
            Some(SetupStage::InstallStarted)
        );
        assert_eq!(parse_setup_stage("complete"), Some(SetupStage::Complete));
        assert_eq!(parse_setup_stage(""), None);
        assert_eq!(parse_setup_stage("garbage"), None);
        assert_eq!(
            setup_stage_name(SetupStage::InstallStarted),
            "install-started"
        );
        assert_eq!(setup_stage_name(SetupStage::Complete), "complete");
    }

    #[test]
    fn stage_transitions_form_the_reboot_loop() {
        let ready = WslStatus::Ready(vec!["Ubuntu".to_string()]);
        // Fresh machine, user accepts: reboot-pending marker.
        assert_eq!(
            stage_to_persist(None, &WslStatus::Absent, true),
            Some(SetupStage::InstallStarted)
        );
        assert!(reboot_pending(
            Some(SetupStage::InstallStarted),
            &WslStatus::Absent
        ));
        // Still no WSL after the install ran: keep waiting for the reboot.
        assert_eq!(
            stage_to_persist(
                Some(SetupStage::InstallStarted),
                &WslStatus::NoDistro,
                false
            ),
            Some(SetupStage::InstallStarted)
        );
        assert!(reboot_pending(
            Some(SetupStage::InstallStarted),
            &WslStatus::NoDistro
        ));
        // Post-reboot: WSL ready → complete, no longer pending.
        assert_eq!(
            stage_to_persist(Some(SetupStage::InstallStarted), &ready, false),
            Some(SetupStage::Complete)
        );
        assert!(!reboot_pending(Some(SetupStage::InstallStarted), &ready));
        // Declined before anything started: nothing persisted.
        assert_eq!(stage_to_persist(None, &WslStatus::Absent, false), None);
        // Complete is sticky.
        assert_eq!(
            stage_to_persist(Some(SetupStage::Complete), &ready, false),
            Some(SetupStage::Complete)
        );
    }
}
