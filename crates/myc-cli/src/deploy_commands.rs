//! `myc deploy`: ship an environment to any Linux server over SSH, moving
//! only the blobs the server is missing, then optionally start it.
//!
//! No agent, no registry, no Docker on the target — just sshd and a `myc`
//! binary (uploaded automatically when absent). The wire protocol lives in
//! `myc_hub::deploy`; this module owns the CLI half: target parsing, the
//! ssh subprocess transport, remote bootstrap, and the `--run` launch.
//!
//! The transport is injectable through `$MYCEL_SSH_CMD` (default `ssh`).
//! Tests set it to e.g. `env MYCEL_STORE=/tmp/remote bash -c`, which runs
//! the "remote" command locally against a separate store — the whole
//! deploy path is exercised without an sshd.

use crate::ui::human_bytes;
use anyhow::{bail, Context, Result};
use myc_store::Store;
use std::io::{BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

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
            bail!("empty deploy target (expected user@host or ssh://user@host[:port])");
        }
        if rest.contains('/') {
            bail!("invalid deploy target '{s}' (paths are not part of a target)");
        }
        // A port is only unambiguous in the ssh:// form: `user@host:2222`.
        // (Plain `host:thing` is scp syntax, which we do not accept.)
        let (dest, port) = if has_scheme {
            match rest.rsplit_once(':') {
                Some((d, p)) if !p.is_empty() && !p.contains('@') => {
                    let port: u16 =
                        p.parse().ok().filter(|p| *p > 0).with_context(|| {
                            format!("invalid port '{p}' in deploy target '{s}'")
                        })?;
                    (d.to_string(), Some(port))
                }
                _ => (rest.to_string(), None),
            }
        } else {
            if rest.contains(':') {
                bail!("invalid deploy target '{s}' — for a custom port use ssh://user@host:port");
            }
            (rest.to_string(), None)
        };
        let host = dest.rsplit_once('@').map(|(_, h)| h).unwrap_or(&dest);
        if host.is_empty() {
            bail!("invalid deploy target '{s}' (no host)");
        }
        Ok(Target { dest, port })
    }
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
    target: Target,
}

impl Transport {
    pub fn new(target: Target) -> Transport {
        let argv = std::env::var("MYCEL_SSH_CMD")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(|| vec!["ssh".to_string()]);
        Transport { argv, target }
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
            if let Some(port) = self.target.port {
                cmd.arg("-p").arg(port.to_string());
            }
            cmd.arg(&self.target.dest).arg(remote_command);
        } else {
            // bash -c convention: script first, then $0.
            cmd.arg(remote_command).arg(&self.target.dest);
        }
        cmd
    }

    /// Run a remote command, feeding it `stdin_data`, capturing stdout.
    /// Returns (exit code, stdout). Stderr passes through to the user.
    fn run(&self, remote_command: &str, stdin_data: Option<&[u8]>) -> Result<(i32, String)> {
        let mut child = self
            .command(remote_command)
            .stdin(if stdin_data.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("cannot start transport '{}'", self.argv.join(" ")))?;
        if let Some(data) = stdin_data {
            let mut stdin = child.stdin.take().expect("stdin was piped");
            stdin.write_all(data)?;
            drop(stdin); // EOF so `cat` on the other side finishes
        }
        let out = child.wait_with_output()?;
        Ok((
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ))
    }

    /// Spawn a long-lived remote command with piped stdin/stdout for the
    /// deploy protocol.
    fn spawn_pipe(&self, remote_command: &str) -> Result<Child> {
        self.command(remote_command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("cannot start transport '{}'", self.argv.join(" ")))
    }
}

/// Single-quote `s` for a POSIX shell.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A ref name as a safe file name for remote pid/log files.
fn safe_name(ref_name: &str) -> String {
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

const BOOTSTRAP_PATH: &str = "$HOME/.local/bin/myc";

/// Everything `myc deploy` needs beyond the ref and the target.
pub struct DeployArgs {
    /// Start the environment on the target after the transfer.
    pub run: bool,
    /// Stop a previous instance of the same ref first (implies --run).
    pub restart: bool,
    /// `-p HOST:CONTAINER` specs forwarded to the remote `myc run`.
    pub publish: Vec<String>,
    /// `-v` mount specs forwarded to the remote `myc run`.
    pub binds: Vec<String>,
    /// `--net` mode forwarded to the remote `myc run`.
    pub net: Option<String>,
    /// Path of the `myc` binary on the target (default: `myc` in PATH,
    /// falling back to `~/.local/bin/myc`).
    pub myc_path: Option<String>,
    /// Never upload a `myc` binary to the target.
    pub no_bootstrap: bool,
}

/// `myc deploy REF TARGET`: negotiate, send the delta, register the ref —
/// then optionally launch it.
pub fn deploy(root: &Path, reference: &str, target_spec: &str, args: DeployArgs) -> Result<i32> {
    let store =
        Store::open(root).with_context(|| format!("cannot open store at {}", root.display()))?;
    let id = store
        .resolve(reference)
        .with_context(|| format!("'{reference}' not found in the local store"))?;
    let manifest = store.get_manifest(&id)?;
    let missing_local = store.missing_blobs(&manifest);
    if !missing_local.is_empty() {
        bail!(
            "{} blobs missing locally; run `myc pull {reference}` before deploying",
            missing_local.len()
        );
    }

    let target = Target::parse(target_spec)?;
    let transport = Transport::new(target);
    let myc = ensure_remote_myc(&transport, &args)?;

    // The transfer itself: local `push_deploy` ⇄ remote `_serve-deploy`.
    let mut child = transport.spawn_pipe(&format!("{myc} _serve-deploy"))?;
    let mut to_remote = child.stdin.take().expect("stdin was piped");
    let mut from_remote = BufReader::new(child.stdout.take().expect("stdout was piped"));
    let report = myc_hub::deploy::push_deploy(
        &store,
        &manifest,
        reference,
        &mut to_remote,
        &mut from_remote,
        &|done, total| {
            eprint!("\rmyc: sending blob {done}/{total}…");
            let _ = std::io::stderr().flush();
        },
    );
    drop(to_remote);
    let status = child.wait()?;
    let report = report.map_err(|e| {
        if status.success() {
            anyhow::anyhow!(e)
        } else {
            anyhow::anyhow!("{e} (remote exited with {status})")
        }
    })?;
    if report.blobs_sent > 0 {
        eprintln!();
    }

    println!(
        "deployed {reference} → {} — {} file{} sent ({}), {} already present — done in {:.1?}",
        transport.target.dest,
        report.blobs_sent,
        if report.blobs_sent == 1 { "" } else { "s" },
        human_bytes(report.bytes_sent),
        report.blobs_total - report.blobs_sent,
        report.elapsed,
    );

    if args.run || args.restart {
        launch_remote(&transport, &myc, reference, &manifest, &args)?;
    }
    Ok(0)
}

/// Find (or install) `myc` on the target. Returns the path to use, shell-
/// quoted where needed (`$HOME` must stay expandable).
fn ensure_remote_myc(transport: &Transport, args: &DeployArgs) -> Result<String> {
    if let Some(path) = &args.myc_path {
        let quoted = sh_quote(path);
        let (code, _) = transport.run(&format!("command -v {quoted} >/dev/null"), None)?;
        if code != 0 {
            bail!(
                "no runnable myc at '{path}' on {} (--myc-path)",
                transport.target.dest
            );
        }
        return Ok(quoted);
    }

    let probe = format!("command -v myc 2>/dev/null || command -v {BOOTSTRAP_PATH} 2>/dev/null");
    let (code, found) = transport.run(&probe, None)?;
    if code == 0 && !found.is_empty() {
        // Use the resolved absolute path: non-interactive ssh sessions often
        // have a minimal PATH.
        return Ok(sh_quote(found.lines().next().unwrap_or("myc").trim()));
    }

    if args.no_bootstrap {
        bail!(
            "myc is not installed on {} and --no-bootstrap was given\n  \
             install it there, or drop --no-bootstrap to upload this machine's binary",
            transport.target.dest
        );
    }

    // Bootstrap: upload the binary this process is running from. On the
    // machines that can deploy (Linux/WSL2) that is a static Linux build —
    // exactly what a Linux server needs.
    let exe = std::env::current_exe().context("cannot locate the current myc binary")?;
    let bytes = std::fs::read(&exe)
        .with_context(|| format!("cannot read {} for bootstrap", exe.display()))?;
    eprintln!(
        "myc: not installed on {} — uploading this machine's binary ({}) to ~/.local/bin/myc…",
        transport.target.dest,
        human_bytes(bytes.len() as u64),
    );
    let install = format!(
        "mkdir -p \"$HOME/.local/bin\" && cat > {BOOTSTRAP_PATH}.tmp && \
         chmod +x {BOOTSTRAP_PATH}.tmp && mv {BOOTSTRAP_PATH}.tmp {BOOTSTRAP_PATH}"
    );
    let (code, _) = transport.run(&install, Some(&bytes))?;
    if code != 0 {
        bail!("bootstrap upload to {} failed", transport.target.dest);
    }
    // Trust, then verify: the uploaded binary must actually run there.
    let (code, _) = transport.run(&format!("{BOOTSTRAP_PATH} --version"), None)?;
    if code != 0 {
        bail!(
            "uploaded myc does not run on {} (different architecture?)\n  \
             install myc there manually and retry with --myc-path",
            transport.target.dest
        );
    }
    eprintln!("myc: bootstrap done — myc installed at ~/.local/bin/myc");
    Ok(BOOTSTRAP_PATH.to_string())
}

/// Start `myc run REF` on the target, detached, with a pidfile under the
/// remote store (`…/deploys/NAME.pid`). `--restart` kills the previous
/// instance of the same ref first.
fn launch_remote(
    transport: &Transport,
    myc: &str,
    reference: &str,
    manifest: &myc_manifest::Manifest,
    args: &DeployArgs,
) -> Result<()> {
    let name = safe_name(reference);
    let mut run_flags = String::new();
    if let Some(net) = &args.net {
        run_flags.push_str(&format!(" --net {}", sh_quote(net)));
    }
    for p in &args.publish {
        run_flags.push_str(&format!(" -p {}", sh_quote(p)));
    }
    for v in &args.binds {
        run_flags.push_str(&format!(" -v {}", sh_quote(v)));
    }

    let stop = if args.restart {
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
    let (code, out) = transport.run(&script, None)?;
    if code != 0 {
        bail!("remote launch of {reference} failed (exit {code})");
    }
    let pid = out
        .lines()
        .find_map(|l| l.strip_prefix("pid="))
        .unwrap_or("?")
        .to_string();

    // Ports worth telling the user about: what was published, or what the
    // image says it listens on (host networking = same port on the server).
    let ports: Vec<String> = if !args.publish.is_empty() {
        args.publish
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
    println!(
        "started {reference} on {} (pid {pid}, log: ~/.mycel/deploys/{name}.log){}",
        transport.target.dest,
        if ports.is_empty() {
            String::new()
        } else {
            format!(
                " — port{} {}",
                if ports.len() == 1 { "" } else { "s" },
                ports.join(", ")
            )
        },
    );
    Ok(())
}

/// `myc _serve-deploy` (hidden plumbing): the remote half of a deploy,
/// speaking the protocol on stdin/stdout. Invoked by `myc deploy` through
/// ssh — never by hand.
pub fn serve_deploy(root: &Path) -> Result<i32> {
    let store =
        Store::open(root).with_context(|| format!("cannot open store at {}", root.display()))?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    myc_hub::deploy::serve_deploy(&store, &mut stdin.lock(), &mut stdout.lock())?;
    Ok(0)
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
}
