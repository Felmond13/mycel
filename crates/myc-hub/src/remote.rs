//! Shared machinery for deploying over SSH: target parsing, the ssh
//! subprocess transport, remote `myc` bootstrap, the transfer driver and
//! the optional remote launch.
//!
//! Two consumers, one code path: the CLI (`myc deploy`, which owns the
//! terminal output) and the web dashboard (`POST /api/deploy`, which turns
//! the same structured [`RemoteError`]s into plain-language messages).
//!
//! The transport is injectable through `$MYCEL_SSH_CMD` (default `ssh`).
//! Tests set it to e.g. `env MYCEL_STORE=/tmp/remote bash -c`, which runs
//! the "remote" command locally against a separate store — the whole
//! deploy path is exercised without an sshd.

use crate::deploy::{DeployError, DeployReport};
use crate::transfer::Progress;
use myc_manifest::Manifest;
use myc_store::Store;
use std::io::{BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// Where the bootstrap installs `myc` on the target (kept as a literal so
/// `$HOME` is expanded by the remote shell, not locally).
pub const BOOTSTRAP_PATH: &str = "$HOME/.local/bin/myc";

/// Everything that can go wrong between "parse the target" and "the app
/// runs on the server", as variants a UI can pattern-match on. `Display`
/// keeps the CLI-grade message for each.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The target string does not parse (`user@host` / `ssh://…` expected).
    #[error("{0}")]
    BadTarget(String),
    /// The transport program (usually `ssh`) could not be started at all —
    /// on a typical machine this means ssh is not installed.
    #[error("cannot start transport '{transport}': {source}")]
    Spawn {
        transport: String,
        #[source]
        source: std::io::Error,
    },
    /// ssh itself failed (exit 255): host unreachable, name that does not
    /// resolve, refused key, host-key mismatch… `detail` carries ssh's own
    /// words when they were captured.
    #[error("cannot reach {dest}: {detail}")]
    Ssh { dest: String, detail: String },
    /// `--myc-path` points at nothing runnable on the target.
    #[error("no runnable myc at '{path}' on {dest} (--myc-path)")]
    MycPathMissing { dest: String, path: String },
    /// The target has no `myc` and bootstrap was disabled.
    #[error(
        "myc is not installed on {dest} and --no-bootstrap was given\n  \
         install it there, or drop --no-bootstrap to upload this machine's binary"
    )]
    BootstrapRefused { dest: String },
    /// Uploading this machine's binary to the target failed.
    #[error("bootstrap upload to {dest} failed")]
    BootstrapUpload { dest: String },
    /// The uploaded binary does not execute on the target.
    #[error(
        "uploaded myc does not run on {dest} (different architecture?)\n  \
         install myc there manually and retry with --myc-path"
    )]
    BootstrapIncompatible { dest: String },
    /// The transfer failed and the remote process died; `detail` includes
    /// the protocol error, the remote exit status and any captured stderr.
    #[error("{detail}")]
    Transfer { detail: String },
    /// Protocol-level failure while the transport itself stayed healthy.
    #[error(transparent)]
    Deploy(#[from] DeployError),
    /// `myc run` on the target exited non-zero before detaching.
    #[error("remote launch failed (exit {code})")]
    Launch { code: i32 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// `std::env::current_exe` failed — bootstrap has nothing to upload.
    #[error("cannot locate the current myc binary: {0}")]
    NoLocalBinary(String),
}

pub type Result<T> = std::result::Result<T, RemoteError>;

fn bad_target(msg: impl Into<String>) -> RemoteError {
    RemoteError::BadTarget(msg.into())
}

/// A parsed deploy destination: `user@host` or `ssh://user@host[:port]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// What ssh calls the destination: `user@host` or `host`.
    pub dest: String,
    /// Explicit port (`ssh://…:2222` form only).
    pub port: Option<u16>,
}

impl Target {
    pub fn parse(s: &str) -> Result<Target> {
        let (rest, has_scheme) = match s.strip_prefix("ssh://") {
            Some(r) => (r, true),
            None => (s, false),
        };
        if rest.is_empty() {
            return Err(bad_target(
                "empty deploy target (expected user@host or ssh://user@host[:port])",
            ));
        }
        if rest.contains('/') {
            return Err(bad_target(format!(
                "invalid deploy target '{s}' (paths are not part of a target)"
            )));
        }
        // A port is only unambiguous in the ssh:// form: `user@host:2222`.
        // (Plain `host:thing` is scp syntax, which we do not accept.)
        let (dest, port) = if has_scheme {
            match rest.rsplit_once(':') {
                Some((d, p)) if !p.is_empty() && !p.contains('@') => {
                    let port: u16 = p.parse().ok().filter(|p| *p > 0).ok_or_else(|| {
                        bad_target(format!("invalid port '{p}' in deploy target '{s}'"))
                    })?;
                    (d.to_string(), Some(port))
                }
                _ => (rest.to_string(), None),
            }
        } else {
            if rest.contains(':') {
                return Err(bad_target(format!(
                    "invalid deploy target '{s}' — for a custom port use ssh://user@host:port"
                )));
            }
            (rest.to_string(), None)
        };
        let host = dest.rsplit_once('@').map(|(_, h)| h).unwrap_or(&dest);
        if host.is_empty() {
            return Err(bad_target(format!("invalid deploy target '{s}' (no host)")));
        }
        Ok(Target { dest, port })
    }
}

/// One finished remote command: exit code, stdout, and stderr (empty when
/// stderr was passed through to the terminal instead of captured).
pub struct Exec {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The remote-execution transport: a subprocess that runs one shell command
/// on the target and wires up its stdin/stdout.
///
/// Default: `ssh [-p PORT] DEST COMMAND`. `$MYCEL_SSH_CMD` overrides the
/// program (whitespace-split). Anything whose basename is not `ssh` is
/// treated as a `bash -c`-style runner and gets `COMMAND DEST` instead
/// (DEST lands in the harmless `$0`) — so tests can point it at
/// `env MYCEL_STORE=/tmp/remote bash -c` and exercise the whole deploy
/// path locally, without an sshd.
pub struct Transport {
    argv: Vec<String>,
    pub target: Target,
    batch: bool,
    capture: bool,
}

impl Transport {
    pub fn new(target: Target) -> Transport {
        let argv = std::env::var("MYCEL_SSH_CMD")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(|| vec!["ssh".to_string()]);
        Transport {
            argv,
            target,
            batch: false,
            capture: false,
        }
    }

    /// Non-interactive mode for servers (the web dashboard): ssh never
    /// prompts (`BatchMode=yes` — a password prompt becomes a clean
    /// failure instead of a hang), first-contact host keys are accepted
    /// (`accept-new`; changed keys still refuse), and dead hosts give up
    /// after 10 s.
    pub fn batch(mut self, yes: bool) -> Transport {
        self.batch = yes;
        self
    }

    /// Capture stderr into [`Exec::stderr`] / error details instead of
    /// letting it flow to this process's terminal.
    pub fn capture_stderr(mut self, yes: bool) -> Transport {
        self.capture = yes;
        self
    }

    /// What ssh calls the destination (`user@host`).
    pub fn dest(&self) -> &str {
        &self.target.dest
    }

    fn describe(&self) -> String {
        self.argv.join(" ")
    }

    fn is_real_ssh(&self) -> bool {
        Path::new(&self.argv[0])
            .file_name()
            .map(|f| f == "ssh")
            .unwrap_or(false)
    }

    /// Build the subprocess for one remote shell command.
    fn command(&self, remote_command: &str) -> Command {
        let mut cmd = Command::new(&self.argv[0]);
        cmd.args(&self.argv[1..]);
        if self.is_real_ssh() {
            if self.batch {
                cmd.args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    "-o",
                    "StrictHostKeyChecking=accept-new",
                ]);
            }
            if let Some(port) = self.target.port {
                cmd.arg("-p").arg(port.to_string());
            }
            cmd.arg(&self.target.dest).arg(remote_command);
        } else {
            // bash -c convention: script first, then $0.
            cmd.arg(remote_command).arg(&self.target.dest);
        }
        if self.capture {
            cmd.stderr(Stdio::piped());
        }
        cmd
    }

    fn spawn_error(&self, source: std::io::Error) -> RemoteError {
        RemoteError::Spawn {
            transport: self.describe(),
            source,
        }
    }

    /// ssh reserves exit code 255 for its own failures (unreachable host,
    /// refused auth, …) as opposed to the remote command's exit code.
    fn ssh_failure(&self, exec: &Exec) -> Option<RemoteError> {
        if self.is_real_ssh() && exec.code == 255 {
            let detail = if exec.stderr.is_empty() {
                "connection failed".to_string()
            } else {
                exec.stderr.clone()
            };
            Some(RemoteError::Ssh {
                dest: self.target.dest.clone(),
                detail,
            })
        } else {
            None
        }
    }

    /// Run a remote command, feeding it `stdin_data`, capturing stdout
    /// (and stderr in capture mode).
    pub fn run(&self, remote_command: &str, stdin_data: Option<&[u8]>) -> Result<Exec> {
        let mut child = self
            .command(remote_command)
            .stdin(if stdin_data.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| self.spawn_error(e))?;
        if let Some(data) = stdin_data {
            let mut stdin = child.stdin.take().expect("stdin was piped");
            stdin.write_all(data)?;
            drop(stdin); // EOF so `cat` on the other side finishes
        }
        let out = child.wait_with_output()?;
        Ok(Exec {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }

    /// Spawn a long-lived remote command with piped stdin/stdout for the
    /// deploy protocol.
    fn spawn_pipe(&self, remote_command: &str) -> Result<Child> {
        self.command(remote_command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| self.spawn_error(e))
    }
}

/// Single-quote `s` for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A ref name as a safe file name for remote pid/log files.
pub fn safe_name(ref_name: &str) -> String {
    ref_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn human_bytes(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

/// How to find (or install) `myc` on the target.
#[derive(Default)]
pub struct BootstrapOptions {
    /// Explicit path of the `myc` binary on the target (skips probing).
    pub myc_path: Option<String>,
    /// Never upload a binary; fail instead when the target has none.
    pub no_bootstrap: bool,
}

/// Find (or install) `myc` on the target. Returns the path to use, shell-
/// quoted where needed (`$HOME` must stay expandable). Human-readable
/// progress ("uploading this machine's binary…") goes through `notice`.
pub fn ensure_remote_myc(
    transport: &Transport,
    opts: &BootstrapOptions,
    notice: &dyn Fn(&str),
) -> Result<String> {
    if let Some(path) = &opts.myc_path {
        let quoted = sh_quote(path);
        let exec = transport.run(&format!("command -v {quoted} >/dev/null"), None)?;
        if let Some(e) = transport.ssh_failure(&exec) {
            return Err(e);
        }
        if exec.code != 0 {
            return Err(RemoteError::MycPathMissing {
                dest: transport.target.dest.clone(),
                path: path.clone(),
            });
        }
        return Ok(quoted);
    }

    let probe = format!("command -v myc 2>/dev/null || command -v {BOOTSTRAP_PATH} 2>/dev/null");
    let exec = transport.run(&probe, None)?;
    if let Some(e) = transport.ssh_failure(&exec) {
        return Err(e);
    }
    if exec.code == 0 && !exec.stdout.is_empty() {
        // Use the resolved absolute path: non-interactive ssh sessions often
        // have a minimal PATH.
        return Ok(sh_quote(exec.stdout.lines().next().unwrap_or("myc").trim()));
    }

    if opts.no_bootstrap {
        return Err(RemoteError::BootstrapRefused {
            dest: transport.target.dest.clone(),
        });
    }

    // Bootstrap: upload the binary this process is running from. On the
    // machines that can deploy (Linux/WSL2) that is a static Linux build —
    // exactly what a Linux server needs.
    let exe = std::env::current_exe().map_err(|e| RemoteError::NoLocalBinary(e.to_string()))?;
    let bytes = std::fs::read(&exe).map_err(|e| {
        RemoteError::NoLocalBinary(format!("cannot read {} for bootstrap: {e}", exe.display()))
    })?;
    notice(&format!(
        "not installed on {} — uploading this machine's binary ({}) to ~/.local/bin/myc…",
        transport.target.dest,
        human_bytes(bytes.len() as u64),
    ));
    let install = format!(
        "mkdir -p \"$HOME/.local/bin\" && cat > {BOOTSTRAP_PATH}.tmp && \
         chmod +x {BOOTSTRAP_PATH}.tmp && mv {BOOTSTRAP_PATH}.tmp {BOOTSTRAP_PATH}"
    );
    let exec = transport.run(&install, Some(&bytes))?;
    if let Some(e) = transport.ssh_failure(&exec) {
        return Err(e);
    }
    if exec.code != 0 {
        return Err(RemoteError::BootstrapUpload {
            dest: transport.target.dest.clone(),
        });
    }
    // Trust, then verify: the uploaded binary must actually run there.
    let exec = transport.run(&format!("{BOOTSTRAP_PATH} --version"), None)?;
    if exec.code != 0 {
        return Err(RemoteError::BootstrapIncompatible {
            dest: transport.target.dest.clone(),
        });
    }
    notice("bootstrap done — myc installed at ~/.local/bin/myc");
    Ok(BOOTSTRAP_PATH.to_string())
}

/// The transfer itself: local `push_deploy` ⇄ remote `myc _serve-deploy`
/// through the transport. `myc` is the (already shell-quoted) remote
/// binary path from [`ensure_remote_myc`].
pub fn run_deploy(
    store: &Store,
    manifest: &Manifest,
    ref_name: &str,
    transport: &Transport,
    myc: &str,
    progress: Progress<'_>,
) -> Result<DeployReport> {
    let mut child = transport.spawn_pipe(&format!("{myc} _serve-deploy"))?;
    let mut to_remote = child.stdin.take().expect("stdin was piped");
    let mut from_remote = BufReader::new(child.stdout.take().expect("stdout was piped"));
    let result = crate::deploy::push_deploy(
        store,
        manifest,
        ref_name,
        &mut to_remote,
        &mut from_remote,
        progress,
    );
    drop(to_remote);
    drop(from_remote);
    // stdin/stdout were taken above, so this only drains captured stderr.
    let out = child.wait_with_output()?;
    match result {
        Ok(report) => Ok(report),
        Err(e) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let code = out.status.code().unwrap_or(-1);
            if transport.is_real_ssh() && code == 255 {
                return Err(RemoteError::Ssh {
                    dest: transport.target.dest.clone(),
                    detail: if stderr.is_empty() {
                        e.to_string()
                    } else {
                        stderr
                    },
                });
            }
            if out.status.success() {
                return Err(RemoteError::Deploy(e));
            }
            let mut detail = format!("{e} (remote exited with {})", out.status);
            if !stderr.is_empty() {
                detail.push_str(&format!(": {stderr}"));
            }
            Err(RemoteError::Transfer { detail })
        }
    }
}

/// Everything the remote `myc run` launch needs beyond the ref.
#[derive(Default)]
pub struct LaunchOptions {
    /// Stop a previous instance of the same ref first.
    pub restart: bool,
    /// `-p HOST:CONTAINER` specs forwarded to the remote `myc run`.
    pub publish: Vec<String>,
    /// `-v` mount specs forwarded to the remote `myc run`.
    pub binds: Vec<String>,
    /// `--net` mode forwarded to the remote `myc run`.
    pub net: Option<String>,
}

/// What a successful remote launch looks like.
pub struct LaunchReport {
    /// Detached pid on the target (`"?"` when it could not be read back).
    pub pid: String,
    /// Safe file-name form of the ref (pid/log files live under
    /// `~/.mycel/deploys/<name>.{pid,log}` on the target).
    pub name: String,
    /// Ports worth telling the user about: the published host ports, or
    /// the image's exposed ports (host networking = same port).
    pub ports: Vec<String>,
}

/// Start `myc run REF` on the target, detached, with a pidfile under the
/// remote store (`…/deploys/NAME.pid`). `restart` kills the previous
/// instance of the same ref first.
pub fn launch_remote(
    transport: &Transport,
    myc: &str,
    reference: &str,
    manifest: &Manifest,
    opts: &LaunchOptions,
) -> Result<LaunchReport> {
    let name = safe_name(reference);
    let mut run_flags = String::new();
    if let Some(net) = &opts.net {
        run_flags.push_str(&format!(" --net {}", sh_quote(net)));
    }
    for p in &opts.publish {
        run_flags.push_str(&format!(" -p {}", sh_quote(p)));
    }
    for v in &opts.binds {
        run_flags.push_str(&format!(" -v {}", sh_quote(v)));
    }

    let stop = if opts.restart {
        format!(
            "if [ -f \"$DIR/{name}.pid\" ]; then \
               OLD=\"$(cat \"$DIR/{name}.pid\")\"; \
               if kill \"$OLD\" 2>/dev/null; then \
                 echo \"myc: stopped previous instance (pid $OLD)\" >&2; \
                 while kill -0 \"$OLD\" 2>/dev/null; do sleep 0.1; done; \
               fi; \
             fi; "
        )
    } else {
        String::new()
    };
    let script = format!(
        "DIR=\"${{MYCEL_STORE:-$HOME/.mycel}}/deploys\"; mkdir -p \"$DIR\"; {stop}\
         nohup {myc} run {} {run_flags} > \"$DIR/{name}.log\" 2>&1 & \
         PID=$!; echo \"$PID\" > \"$DIR/{name}.pid\"; echo \"pid=$PID\"",
        sh_quote(reference),
    );
    let exec = transport.run(&script, None)?;
    if let Some(e) = transport.ssh_failure(&exec) {
        return Err(e);
    }
    if exec.code != 0 {
        return Err(RemoteError::Launch { code: exec.code });
    }
    let pid = exec
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("pid="))
        .unwrap_or("?")
        .to_string();

    let ports: Vec<String> = if !opts.publish.is_empty() {
        opts.publish
            .iter()
            .map(|p| p.split(':').next().unwrap_or(p).to_string())
            .collect()
    } else {
        manifest
            .config
            .exposed_ports
            .iter()
            .map(u16::to_string)
            .collect()
    };
    Ok(LaunchReport { pid, name, ports })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_user_at_host() {
        assert_eq!(
            Target::parse("deploy@vps.example.com").unwrap(),
            Target {
                dest: "deploy@vps.example.com".into(),
                port: None
            }
        );
        assert_eq!(
            Target::parse("just-a-host").unwrap(),
            Target {
                dest: "just-a-host".into(),
                port: None
            }
        );
    }

    #[test]
    fn target_ssh_url_with_port() {
        assert_eq!(
            Target::parse("ssh://deploy@vps:2222").unwrap(),
            Target {
                dest: "deploy@vps".into(),
                port: Some(2222)
            }
        );
        assert_eq!(
            Target::parse("ssh://deploy@vps").unwrap(),
            Target {
                dest: "deploy@vps".into(),
                port: None
            }
        );
        assert_eq!(
            Target::parse("ssh://vps:22").unwrap(),
            Target {
                dest: "vps".into(),
                port: Some(22)
            }
        );
    }

    #[test]
    fn target_rejects_garbage() {
        assert!(Target::parse("").is_err());
        assert!(Target::parse("ssh://").is_err());
        assert!(Target::parse("host:2222").is_err()); // port needs ssh://
        assert!(Target::parse("ssh://host:0").is_err());
        assert!(Target::parse("ssh://host:99999").is_err());
        assert!(Target::parse("user@").is_err());
        assert!(Target::parse("user@host/path").is_err());
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote("simple"), "'simple'");
        assert_eq!(sh_quote("with space"), "'with space'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn safe_names_for_pidfiles() {
        assert_eq!(safe_name("web:v2"), "web_v2");
        assert_eq!(safe_name("ghcr.io/org/app:1.2"), "ghcr.io_org_app_1.2");
    }

    #[test]
    fn human_bytes_rounds_sensibly() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
    }
}
