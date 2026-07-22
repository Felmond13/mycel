//! Deploy endpoints: ship an environment to any Linux server over SSH,
//! straight from the Library. This is the web face of `myc deploy`, built
//! on the same shared machinery (`myc_hub::remote`: injectable ssh
//! transport, remote bootstrap, delta transfer, optional launch).
//!
//! A deploy can take a while on first contact (bootstrap upload + every
//! blob), so it runs like an ingest: `POST /api/deploy` answers with a job
//! id immediately and the frontend polls `GET /api/deploy/{id}` for
//! progress and the final stats.
//!
//! Because the server is non-interactive, ssh runs in batch mode: it can
//! only authenticate with keys (the WSL/Linux user's `~/.ssh`), never a
//! password — the error mapping below turns that refusal into a short
//! how-to instead of ssh's bark.

use crate::containers::PortMapping;
use crate::{
    bad_request, blocking, check_reference, internal, not_found, open_store, resolve_installed,
    ApiError, ApiResult, App,
};
use axum::extract::{Path as UrlPath, State};
use axum::Json;
use myc_hub::remote::{
    ensure_remote_myc, launch_remote, run_deploy, BootstrapOptions, LaunchOptions, RemoteError,
    Target, Transport,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// One deploy, as the frontend polls it.
#[derive(Clone, Serialize)]
pub(crate) struct DeployJob {
    #[serde(rename = "ref")]
    reference: String,
    target: String,
    /// `running`, `done` or `failed`.
    status: String,
    /// Human progress line ("sending file 3 of 12…") or the failure
    /// explanation.
    message: String,
    created_at: u64,
    /// Files to send this time (what the server was missing); 0 until the
    /// negotiation answered.
    blobs_total: usize,
    blobs_done: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<DeployResult>,
}

/// Final stats of a successful deploy.
#[derive(Clone, Serialize)]
struct DeployResult {
    manifest_id: String,
    /// Every blob the environment references.
    blobs_total: usize,
    /// Blobs actually sent (the server was missing them).
    blobs_sent: usize,
    blobs_already_present: usize,
    bytes_total: u64,
    bytes_sent: u64,
    elapsed_ms: u64,
    /// Whether the app was (re)started on the server afterwards.
    started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<String>,
    /// Ports the app answers on server-side (published or exposed).
    ports: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct DeployBody {
    #[serde(rename = "ref")]
    reference: String,
    /// `user@host` or `ssh://user@host[:port]`.
    target: String,
    /// Start the app on the server after the transfer.
    #[serde(default)]
    run: bool,
    /// Stop a previous instance of the same ref first (implies `run`).
    #[serde(default)]
    restart: bool,
    /// Host→container port choices for the remote launch. A mapping whose
    /// host equals the app's own port needs nothing (host networking); a
    /// different host port becomes `-p HOST:CONTAINER` on the remote
    /// `myc run` (which switches it to an isolated network).
    #[serde(default)]
    ports: Vec<PortMapping>,
}

/// Turn the user's port choices into remote `myc run` flags. No mapping
/// changes anything on the host network; as soon as one host port differs
/// from the app's own, every mapping is published explicitly (`-p` implies
/// an isolated network on the server, so unlisted ports would go dark).
fn plan_publish(ports: &[PortMapping]) -> Vec<String> {
    if ports.iter().all(|m| m.host == m.container) {
        return Vec::new();
    }
    ports
        .iter()
        .map(|m| format!("{}:{}", m.host, m.container))
        .collect()
}

fn check_ports(ports: &[PortMapping]) -> Result<(), ApiError> {
    if ports.len() > 32 {
        return Err(bad_request("too many port mappings (max 32)"));
    }
    if let Some(m) = ports.iter().find(|m| m.host == 0 || m.container == 0) {
        return Err(bad_request(format!(
            "invalid port mapping {}:{} (ports are 1-65535)",
            m.host, m.container
        )));
    }
    Ok(())
}

/// The pedagogy: every way a deploy can fail, explained in one or two
/// plain sentences with the fix. ssh details are matched on the words the
/// OpenSSH client actually prints.
fn humanize_remote_error(e: &RemoteError) -> String {
    match e {
        RemoteError::BadTarget(msg) => {
            format!("{msg} — it should look like user@host, e.g. deploy@vps.example.com")
        }
        RemoteError::Spawn { .. } => "ssh is not installed on this machine — the dashboard \
             uses the `ssh` command to reach servers. Install it (Ubuntu/Debian: \
             `sudo apt install openssh-client`), then try again."
            .to_string(),
        RemoteError::Ssh { dest, detail } => humanize_ssh_detail(dest, detail),
        RemoteError::BootstrapUpload { dest } => format!(
            "Could not install Mycel on {dest} — the upload was refused. Check that this \
             account can write to its own home directory there, or install myc on the \
             server yourself and try again."
        ),
        RemoteError::BootstrapIncompatible { dest } => format!(
            "{dest} could not run the uploaded myc binary — it is probably a different \
             kind of machine (not x86-64 Linux). Install a matching myc build there, \
             then deploy again."
        ),
        RemoteError::Deploy(myc_hub::deploy::DeployError::Remote(msg)) => {
            format!("The server reported a problem: {msg}")
        }
        RemoteError::Launch { code } => format!(
            "The app arrived on the server, but starting it failed (exit {code}). \
             Its log on the server has the reason: ~/.mycel/deploys/<app>.log"
        ),
        other => other.to_string(),
    }
}

/// One plain-language sentence per classic ssh failure.
fn humanize_ssh_detail(dest: &str, detail: &str) -> String {
    let lower = detail.to_lowercase();
    if lower.contains("permission denied") {
        return format!(
            "{dest} did not accept this machine's SSH key — and the dashboard can never \
             type a password. Set up a key first; in a terminal: `ssh-keygen -t ed25519` \
             (once), then `ssh-copy-id {dest}`, then deploy again."
        );
    }
    if lower.contains("could not resolve hostname") {
        return format!(
            "Can't find a server called that — check the address ({dest}). It should \
             look like user@host, e.g. deploy@vps.example.com."
        );
    }
    if lower.contains("connection refused") {
        return format!(
            "The machine answered, but nothing is listening for SSH — is the SSH \
             server (sshd) running on {dest}, and on the right port?"
        );
    }
    if lower.contains("timed out") || lower.contains("no route to host") {
        return format!(
            "No answer from {dest} — the server may be off, or a firewall is blocking \
             SSH. Check from a terminal first: `ssh {dest}`"
        );
    }
    if lower.contains("host key verification failed")
        || lower.contains("host identification has changed")
    {
        return "The server's identity does not match what this machine remembers (this \
                happens after a server reinstall). If that's expected, remove the old \
                entry with `ssh-keygen -R <host>` and deploy again."
            .to_string();
    }
    format!("Could not connect to {dest}: {detail}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

type Jobs = Arc<Mutex<HashMap<u64, DeployJob>>>;

fn update_job(jobs: &Jobs, id: u64, f: impl FnOnce(&mut DeployJob)) {
    if let Some(job) = jobs.lock().unwrap().get_mut(&id) {
        f(job);
    }
}

/// The whole deploy, run on a dedicated thread (ssh + blob streaming are
/// blocking and can take minutes on first contact).
#[allow(clippy::too_many_arguments)]
fn deploy_worker(
    store: myc_store::Store,
    manifest: myc_manifest::Manifest,
    reference: String,
    target: Target,
    run: bool,
    restart: bool,
    publish: Vec<String>,
    jobs: Jobs,
    job_id: u64,
) -> Result<DeployResult, RemoteError> {
    let transport = Transport::new(target).batch(true).capture_stderr(true);
    update_job(&jobs, job_id, |j| {
        j.message = format!("connecting to {}…", transport.dest());
    });

    let myc = ensure_remote_myc(&transport, &BootstrapOptions::default(), &|msg| {
        update_job(&jobs, job_id, |j| {
            j.message = format!("myc {msg}");
        });
    })?;

    let progress_jobs = Arc::clone(&jobs);
    let report = run_deploy(
        &store,
        &manifest,
        &reference,
        &transport,
        &myc,
        &move |done, total| {
            update_job(&progress_jobs, job_id, |j| {
                j.blobs_total = total;
                j.blobs_done = done;
                j.message = format!("sending file {done} of {total}…");
            });
        },
    )?;

    let mut started = false;
    let mut pid = None;
    let mut ports = Vec::new();
    if run || restart {
        update_job(&jobs, job_id, |j| {
            j.message = format!("starting the app on {}…", transport.dest());
        });
        let launch = launch_remote(
            &transport,
            &myc,
            &reference,
            &manifest,
            &LaunchOptions {
                restart,
                publish,
                binds: Vec::new(),
                net: None,
            },
        )?;
        started = true;
        pid = Some(launch.pid);
        ports = launch.ports;
    }

    Ok(DeployResult {
        manifest_id: report.manifest_id,
        blobs_total: report.blobs_total,
        blobs_sent: report.blobs_sent,
        blobs_already_present: report.blobs_total - report.blobs_sent,
        bytes_total: report.bytes_total,
        bytes_sent: report.bytes_sent,
        elapsed_ms: report.elapsed.as_millis() as u64,
        started,
        pid,
        ports,
    })
}

/// `POST /api/deploy`: validate, answer with a job id immediately, and run
/// the deploy on its own thread. Body: `{"ref", "target", "run"?,
/// "restart"?, "ports"?: [{"host", "container"}]}`.
pub(crate) async fn start_deploy(
    State(app): State<App>,
    Json(body): Json<DeployBody>,
) -> ApiResult {
    let reference = body.reference.trim().to_string();
    check_reference(&reference)?;
    let target =
        Target::parse(body.target.trim()).map_err(|e| bad_request(humanize_remote_error(&e)))?;
    check_ports(&body.ports)?;
    let run = body.run || body.restart;
    let restart = body.restart;
    let publish = plan_publish(&body.ports);

    blocking(move || {
        let store = open_store(&app.root)?;
        let manifest_id = resolve_installed(&store, &reference)?;
        let manifest = store.get_manifest(&manifest_id).map_err(internal)?;
        let missing = store.missing_blobs(&manifest).len();
        if missing > 0 {
            return Err(bad_request(format!(
                "'{reference}' is incomplete on this machine ({missing} files missing) — \
                 complete it before deploying"
            )));
        }

        let job_id = app.next_deploy.fetch_add(1, Ordering::SeqCst);
        app.deploy_jobs.lock().unwrap().insert(
            job_id,
            DeployJob {
                reference: reference.clone(),
                target: target.dest.clone(),
                status: "running".to_string(),
                message: "connecting…".to_string(),
                created_at: now_secs(),
                blobs_total: 0,
                blobs_done: 0,
                result: None,
            },
        );

        let jobs = Arc::clone(&app.deploy_jobs);
        std::thread::spawn(move || {
            let outcome = deploy_worker(
                store,
                manifest,
                reference,
                target,
                run,
                restart,
                publish,
                Arc::clone(&jobs),
                job_id,
            );
            let mut jobs = jobs.lock().unwrap();
            if let Some(job) = jobs.get_mut(&job_id) {
                match outcome {
                    Ok(result) => {
                        job.status = "done".to_string();
                        job.message = format!("deployed {} to {}", job.reference, job.target);
                        job.blobs_done = job.blobs_total;
                        job.result = Some(result);
                    }
                    Err(e) => {
                        job.status = "failed".to_string();
                        job.message = humanize_remote_error(&e);
                    }
                }
            }
        });

        Ok(Json(json!({ "job": job_id })))
    })
    .await
}

/// `GET /api/deploy/{id}`: poll one deploy job.
pub(crate) async fn deploy_status(State(app): State<App>, UrlPath(id): UrlPath<u64>) -> ApiResult {
    let jobs = app.deploy_jobs.lock().unwrap();
    match jobs.get(&id) {
        Some(job) => Ok(Json(serde_json::to_value(job).map_err(internal)?)),
        None => Err(not_found(format!("no deploy job {id}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(host: u16, container: u16) -> PortMapping {
        PortMapping { host, container }
    }

    #[test]
    fn same_port_choices_need_no_flags() {
        assert!(plan_publish(&[]).is_empty());
        assert!(plan_publish(&[map(6379, 6379)]).is_empty());
    }

    #[test]
    fn changed_port_publishes_every_mapping() {
        assert_eq!(plan_publish(&[map(8080, 80)]), vec!["8080:80"]);
        // One changed port drags the untouched one along (isolated network
        // on the server would otherwise hide it).
        assert_eq!(
            plan_publish(&[map(8080, 80), map(443, 443)]),
            vec!["8080:80", "443:443"]
        );
    }

    #[test]
    fn port_zero_is_rejected() {
        assert!(check_ports(&[map(0, 80)]).is_err());
        assert!(check_ports(&[map(80, 0)]).is_err());
        assert!(check_ports(&[map(8080, 80)]).is_ok());
    }

    #[test]
    fn password_auth_gets_the_key_setup_recipe() {
        let e = RemoteError::Ssh {
            dest: "deploy@vps".into(),
            detail: "deploy@vps: Permission denied (publickey,password).".into(),
        };
        let msg = humanize_remote_error(&e);
        assert!(msg.contains("ssh-keygen -t ed25519"), "{msg}");
        assert!(msg.contains("ssh-copy-id deploy@vps"), "{msg}");
        assert!(msg.contains("never"), "{msg}");
    }

    #[test]
    fn unknown_host_and_unreachable_are_explained() {
        let resolve = humanize_remote_error(&RemoteError::Ssh {
            dest: "me@tpyo".into(),
            detail: "ssh: Could not resolve hostname tpyo: Name or service not known".into(),
        });
        assert!(resolve.contains("check the address"), "{resolve}");

        let timeout = humanize_remote_error(&RemoteError::Ssh {
            dest: "me@10.0.0.9".into(),
            detail: "ssh: connect to host 10.0.0.9 port 22: Connection timed out".into(),
        });
        assert!(timeout.contains("firewall"), "{timeout}");

        let refused = humanize_remote_error(&RemoteError::Ssh {
            dest: "me@host".into(),
            detail: "ssh: connect to host host port 22: Connection refused".into(),
        });
        assert!(refused.contains("sshd"), "{refused}");
    }

    #[test]
    fn missing_ssh_binary_is_explained() {
        let e = RemoteError::Spawn {
            transport: "ssh".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "No such file"),
        };
        let msg = humanize_remote_error(&e);
        assert!(msg.contains("openssh-client"), "{msg}");
    }

    #[test]
    fn changed_host_key_is_explained() {
        let e = RemoteError::Ssh {
            dest: "me@host".into(),
            detail: "Host key verification failed.".into(),
        };
        let msg = humanize_remote_error(&e);
        assert!(msg.contains("ssh-keygen -R"), "{msg}");
    }

    #[test]
    fn remote_reported_errors_pass_through() {
        let e = RemoteError::Deploy(myc_hub::deploy::DeployError::Remote("disk full".into()));
        assert!(humanize_remote_error(&e).contains("disk full"));
    }

    #[test]
    fn bad_targets_show_the_expected_shape() {
        let e = Target::parse("host:2222").unwrap_err();
        let msg = humanize_remote_error(&e);
        assert!(msg.contains("user@host"), "{msg}");
    }
}
