//! Windows entry point: `myc.exe` as a transparent front for the WSL2 twin.
//!
//! Containers are Linux processes, so on Windows the default behavior is to
//! proxy the ENTIRE command line to a Linux `myc` inside WSL2 (see
//! `myc_backend::wsl`). That gives Windows users the complete feature set —
//! ingest, run, build, lazy streaming, hub, web UI — against ONE store
//! (`~/.mycel` inside WSL, on native ext4 where hardlinks are cheap).
//!
//! What is NOT proxied:
//! - `myc doctor`: Windows-specific checks (WSL2? distro? twin provisioned
//!   and version-matched?) followed by the twin's own Linux-side checks.
//! - `--no-proxy` anywhere on the command line: escape hatch running the
//!   native Windows build against a host-side store (portable commands
//!   only: ls/inspect/stats/gc/…; containers cannot run natively).
//! - An explicit `--store PATH`: a Windows path is meaningless inside WSL,
//!   so it implies native execution.
//! - Pure `--help`/`--version`: answered locally for speed.
//!
//! Double-click from Explorer (no arguments AND this process is the only
//! one attached to its console) is treated as "open the dashboard": start
//! the proxied `ui` server on :7777 (unless one is already listening) and
//! open the default browser on it. From an interactive shell, bare `myc`
//! keeps printing the welcome text as before.

use crate::ui::Paint;
use myc_backend::wsl::Wsl2;
use myc_backend::ExecBackend;
use std::ffi::OsString;
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub enum Preflight {
    /// Command fully handled (proxied or doctored): exit with this code.
    Exit(i32),
    /// Run the native clap CLI with these args (argv[0] included).
    Native(Vec<OsString>),
}

pub fn preflight() -> Preflight {
    let mut args: Vec<OsString> = std::env::args_os().collect();

    // Escape hatch: strip --no-proxy and run the native Windows CLI.
    if args.iter().any(|a| a == "--no-proxy") {
        args.retain(|a| a != "--no-proxy");
        return Preflight::Native(args);
    }
    // A Windows --store path cannot be used inside WSL: implies native.
    if args
        .iter()
        .any(|a| a == "--store" || a.to_string_lossy().starts_with("--store="))
    {
        return Preflight::Native(args);
    }

    let forwarded: Vec<OsString> = args.iter().skip(1).cloned().collect();
    let subcommand = forwarded
        .iter()
        .filter_map(|a| a.to_str())
        .find(|a| !a.starts_with('-'))
        .map(str::to_string);

    match subcommand.as_deref() {
        // Double-click from Explorer: no args and we own the console alone
        // (a shell would be attached too). Launch the dashboard instead of
        // flashing a welcome text nobody can read.
        None if forwarded.is_empty() && launched_by_double_click() => {
            Preflight::Exit(dashboard_launch())
        }
        // Top-level --help/--version (or anything flag-only): answer locally.
        None if !forwarded.is_empty() => Preflight::Native(args),
        // Windows-aware doctor.
        Some("doctor") => Preflight::Exit(doctor()),
        // Everything else — including bare `myc` — goes to the twin.
        _ => {
            if subcommand.as_deref() == Some("ui") {
                let port = port_arg(&forwarded).unwrap_or(7777);
                eprintln!(
                    "myc: dashboard runs inside WSL2; open http://localhost:{port} from Windows"
                );
            }
            Preflight::Exit(proxy(&forwarded))
        }
    }
}

/// The value following `--port` (or `--port=N`), if any.
fn port_arg(args: &[OsString]) -> Option<u16> {
    let mut it = args.iter().filter_map(|a| a.to_str());
    while let Some(a) = it.next() {
        if a == "--port" {
            return it.next()?.parse().ok();
        }
        if let Some(v) = a.strip_prefix("--port=") {
            return v.parse().ok();
        }
    }
    None
}

// Direct kernel32 declaration — one function is not worth a windows-sys
// dependency. Fills `list` with the PIDs attached to this console and
// returns how many there are (0 = no console at all).
#[link(name = "kernel32")]
extern "system" {
    fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
}

/// True when this process is the ONLY one attached to its console: Explorer
/// double-click (and equivalents like Win+R) spawn a fresh console just for
/// us, while an interactive cmd/PowerShell stays attached alongside.
fn launched_by_double_click() -> bool {
    let mut pids = [0u32; 2];
    let n = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    n == 1
}

const DASHBOARD_PORT: u16 = 7777;

/// Double-click behavior: bring up the dashboard and a browser tab on it.
///
/// The `ui` proxy blocks for the lifetime of the server, keeping the console
/// window open. Closing that window (or Ctrl-C) terminates myc.exe and its
/// `wsl.exe` child, and WSL tears the Linux-side server down with its relay,
/// so no orphan keeps the port. If a dashboard is already listening on
/// :7777, we only open the browser on it — and keep the console visible for
/// a few seconds, because a window that flashes and vanishes reads as a
/// crash to someone who just double-clicked an icon.
fn dashboard_launch() -> i32 {
    let url = format!("http://localhost:{DASHBOARD_PORT}");
    if dashboard_listening() {
        println!("Mycel dashboard is already running — opening your browser… ({url})");
        open_browser(&url);
        println!();
        pause_console(5);
        return 0;
    }
    println!("Starting the Mycel dashboard… ({url})");
    println!("Your browser opens automatically once it is ready.");
    println!(
        "Keep this window open while you use Mycel — closing it (or Ctrl-C) stops the dashboard."
    );
    // The server needs a moment inside WSL; open the browser once it answers.
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if dashboard_listening() {
                open_browser(&url);
                return;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    });
    let code = proxy(&[
        OsString::from("ui"),
        OsString::from("--port"),
        OsString::from(DASHBOARD_PORT.to_string()),
    ]);
    if code != 0 {
        // Something went wrong inside WSL: give the user time to read the
        // error above instead of snapping the window shut.
        println!();
        println!("The dashboard could not start (see the message above; `myc doctor` can help).");
        pause_console(15);
    }
    code
}

/// Hold the console window open for `secs` seconds so double-click users
/// can read what happened before the window disappears.
fn pause_console(secs: u64) {
    println!("This window closes in {secs} seconds.");
    std::thread::sleep(Duration::from_secs(secs));
}

fn dashboard_listening() -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], DASHBOARD_PORT));
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

/// `cmd /C start "" URL` — the reliable "default browser" shim on Windows.
fn open_browser(url: &str) {
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn proxy(args: &[OsString]) -> i32 {
    let wsl = Wsl2::from_env();
    match wsl.proxy(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("myc: error: {e:#}");
            eprintln!(
                "myc: hint: `myc doctor` diagnoses the Windows/WSL2 setup; \
                 `wsl --install -d Ubuntu` installs WSL2"
            );
            1
        }
    }
}

/// `myc doctor` on Windows: host-side backend checks first (same format as
/// the Linux doctor), then — when the backend is healthy — the twin's own
/// `myc doctor` for the Linux-side picture.
fn doctor() -> i32 {
    let paint = Paint::auto();
    let wsl = Wsl2::from_env();
    let report = wsl.probe(&myc_store::Store::default_root());

    println!(
        "{}",
        paint.bold("Windows host checks (backend: wsl2 — commands proxy to a Linux twin):")
    );
    let mut failures = 0;
    for check in &report.checks {
        let verdict = if check.ok {
            paint.green("PASS")
        } else {
            failures += 1;
            paint.red("FAIL")
        };
        println!("{verdict}  {:<18} {}", check.name, check.detail);
        if let Some(fix) = &check.fix {
            println!("      {}", paint.yellow(&format!("fix: {fix}")));
        }
    }
    println!();
    if !report.available {
        println!(
            "{}",
            paint.red(&format!("{failures} check(s) failed — see fixes above"))
        );
        return 1;
    }
    println!(
        "{}",
        paint.bold("Checks inside WSL2 (proxied `myc doctor`):")
    );
    match wsl.proxy(&[OsString::from("doctor")]) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("myc: error: {e:#}");
            1
        }
    }
}
