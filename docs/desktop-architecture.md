# Desktop architecture: Mycel on Windows and macOS

Mycel containers are Linux processes (user namespaces, `pivot_root`, FUSE).
Making Mycel "multi-OS" therefore means answering one question per platform:
*where does the Linux part run, and how transparent can we make it?*

This document describes the shipped Windows support (v1) and the planned
macOS support (scaffold only — untested, and honestly labelled as such).

## The backend abstraction

`crates/myc-backend` defines the seam:

```rust
pub trait ExecBackend {
    fn name(&self) -> &'static str;
    fn probe(&self, store_root: &Path) -> BackendReport; // available? fix hints?
    fn run(&self, store_root: &Path, request: &RunRequest) -> Result<i32>;
}
```

| Backend       | Platform | Status | Strategy |
|---------------|----------|--------|----------|
| `native-linux`| Linux    | shipped | `myc-run` user namespaces, exactly as always |
| `wsl2`        | Windows  | shipped | transparent proxy to a Linux twin inside WSL2 |
| `vm`          | macOS    | scaffold | lightweight Linux VM (Virtualization.framework) — planned |

`probe()` never panics: a broken setup yields `available: false` plus
per-check `fix:` lines, which is what `myc doctor` prints.

## Windows (shipped): one .exe, a Linux twin, one store

`myc.exe` is a *transparent front* for a full Linux `myc` running inside
WSL2:

1. **Embedded twin.** Release builds set `MYCEL_LINUX_BINARY`, embedding the
   ~9 MB Linux binary into `myc.exe` via `include_bytes!` (build script of
   `myc-backend`). The .exe is fully self-contained.
2. **Provisioning.** On every command the twin's sha256 inside WSL
   (`~/.mycel-bin/myc`) is compared against the desired bytes; when missing
   or stale it is (re)provisioned by streaming the bytes through
   `wsl.exe … sh -c 'cat > ~/.mycel-bin/myc'`. Upgrading `myc.exe` therefore
   self-heals the twin on its next run. Overrides:
   - `MYCEL_WSL_BINARY`: path to a Linux `myc` build (a `/…` WSL path is
     copied inside WSL; a Windows path is streamed in) — useful for dev.
   - `MYCEL_WSL_DISTRO`: distro selection (default: first entry of
     `wsl -l -q`).
3. **Proxying.** Every command line is forwarded verbatim:
   `wsl.exe -d <distro> --exec sh -c 'exec "$HOME/.mycel-bin/myc" "$@"' myc <args…>`
   — `--exec` skips the WSL shell join/re-split, and `"$@"` preserves each
   argument byte-for-byte, so quoting survives. Exit codes propagate through
   `wsl.exe`. `MYCEL_HUB`/`MYCEL_HUB_TOKEN` are forwarded via `WSLENV`.
4. **One store, inside WSL.** The twin uses its own `~/.mycel` on ext4 —
   hardlink materialization stays free, FUSE lazy streaming works, and the
   web UI (`myc.exe ui`) runs inside WSL while WSL2 localhost forwarding
   makes `http://localhost:7777` reachable from Windows browsers.

Not proxied:

- `myc doctor` — Windows-side checks (WSL2 present? distro? twin provisioned
  and version-matched?) followed by the twin's Linux-side checks.
- `--no-proxy` (or an explicit `--store` with a Windows path) — escape hatch
  running the native Windows build against a host-side store. Portable
  commands work (`ls`, `inspect`, `stats`, `gc`, `export`, `import`,
  `verify`, `diff`, `which`, `sbom`, ingest of manifests/blobs); containers,
  builds and FUSE do not (clear errors point back to the proxy).
- `--help`/`--version` — answered locally.

Reduced semantics on the native (`--no-proxy`) path are documented in code:
no symlink materialization, no process groups (stop = `taskkill /T /F`),
`/proc` metrics empty.

## macOS (planned): lightweight VM — scaffold only

Nothing here is shipped or tested; `myc` built for macOS returns a clear
"not yet available" error pointing at this document.

Planned host/guest split (same shape as Docker Desktop / OrbStack / colima,
but radically smaller because the guest only needs `myc`):

- **Guest image**: a pinned mainline kernel built with a minimal config
  (virtio, ext4, FUSE, user namespaces; ~10–15 MB) plus an initramfs
  containing busybox-init and the Linux `myc` binary. Boot target:
  < 1 second to a serial-console-ready runtime.
- **Hypervisor**: `Virtualization.framework` on macOS (arm64 + x86_64) via
  the `vz` crate. (The same split later enables a Windows Hypervisor
  Platform backend for Windows machines without WSL2.)
- **Store sharing**: the host store directory is exported to the guest over
  **virtiofs**; blob writes happen guest-side, manifests stay host-readable.
  Hardlink materialization runs entirely inside the guest on a small ext4
  scratch disk to keep rootfs setup fast; the content store itself remains
  the single source of truth on the host.
- **Proxying**: identical UX to Windows — `myc` on macOS forwards commands
  to the twin over a vsock; `probe()` reports VM state with fix hints.
- **Licensing note**: the Linux kernel is GPLv2. Distributing a pinned
  kernel image alongside (not linked into) the Apache-2.0 `myc` binary is
  fine, but the kernel config + sources (or an upstream source offer) must
  ship with releases, and the kernel must stay a separate artifact rather
  than bytes embedded into the Mach-O binary.

## Cross-target build matrix

| Target | How | Status |
|--------|-----|--------|
| `x86_64-unknown-linux-gnu` | `scripts/dev.sh release` | shipped, tested (CI gate) |
| `x86_64-pc-windows-msvc` / `-gnu` | `scripts/build-windows.ps1` (needs a C toolchain: VS Build Tools for msvc, mingw-w64 for gnu) | shipped, tested end-to-end via WSL2 proxy |
| `aarch64-apple-darwin` / `x86_64-apple-darwin` | `cargo build --release -p myc-cli` on a Mac | compiles in theory (all Windows fixes are `cfg`-based), **untested — no macOS hardware/SDK in CI yet**; runtime returns "not yet available" |

Windows packaging recipe (what `scripts/build-windows.ps1` automates):

```powershell
# 1. build the Linux twin inside WSL
wsl -d Ubuntu -- bash scripts/dev.sh release
# 2. embed it and build the .exe
$env:MYCEL_LINUX_BINARY = "<windows path to target/release/myc>"
cargo build --release -p myc-cli
```

CI matrix sketch (GitHub Actions):

```yaml
jobs:
  linux:   { runs-on: ubuntu-latest,  run: scripts/dev.sh test && scripts/e2e.sh }
  windows: { runs-on: windows-latest, run: scripts/build-windows.ps1 && scripts/e2e-windows.ps1 }
  macos:   { runs-on: macos-latest,   run: cargo build --release -p myc-cli }   # compile-only, untested
```
