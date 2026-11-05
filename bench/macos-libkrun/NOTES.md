# libkrun × mycel — investigation notes (macOS tier, `hvf-microvm`)

Survey date: 2026-11-05. Author context: written from a **Windows/WSL2
workstation — no Apple hardware**. Everything below is either (a) read
from primary sources (the libkrun v1.19.7 header, checked out at the
tag), (b) taken from the archived Capsule M0 investigation (2026-11-03,
`/home/nboukadoum-ext/capsule`), or (c) explicitly marked UNCERTAIN.
Nothing here has been executed on a Mac.

Primary source of truth: `include/libkrun.h` of tag `v1.19.7`
(https://github.com/containers/libkrun — the repo now also lives at
github.com/libkrun/libkrun; v1.19.7 is the stable 1.x line the bindings
target). A verbatim copy used for the survey is quoted inline below; the
engine's Rust FFI (`crates/myc-backend/src/vm.rs`) mirrors it.

---

## 1. The API actually used by `vm.rs` (exact signatures, v1.19.7)

All return `int32_t`: **zero on success, a negative `-errno` on
failure**. `ctx_id` comes from `krun_create_ctx`.

```c
int32_t krun_set_log_level(uint32_t level);            // 0=off … 4=debug
int32_t krun_create_ctx();                             // returns ctx_id >= 0
int32_t krun_free_ctx(uint32_t ctx_id);
int32_t krun_set_vm_config(uint32_t ctx_id, uint8_t num_vcpus, uint32_t ram_mib);
int32_t krun_set_kernel(uint32_t ctx_id,
                        const char *kernel_path,       // host path
                        uint32_t kernel_format,        // see §3
                        const char *initramfs,         // NULL = none
                        const char *cmdline);
int32_t krun_set_root(uint32_t ctx_id, const char *root_path);   // host DIR → virtiofs guest root ("/dev/root")
int32_t krun_set_workdir(uint32_t ctx_id, const char *workdir_path); // relative to the root share
int32_t krun_set_exec(uint32_t ctx_id,
                      const char *exec_path,           // relative to the root
                      const char *const argv[],        // NUL-terminated array
                      const char *const envp[]);       // NUL-terminated array; NULL = inherit host env (DO NOT RELY for containers)
int32_t krun_start_enter(uint32_t ctx_id);             // DOES NOT RETURN on success — see §2
```

Why dlopen and not link-time `#[link(name = "krun")]`: `myc` must start
and run an honest `myc doctor` on Macs WITHOUT libkrun installed; a
link-time dependency would make the whole binary fail to load. The
library is opened with `dlopen(path, RTLD_NOW|RTLD_LOCAL)` (macOS
constants: NOW=0x2, LOCAL=0x4 — **different from Linux where LOCAL=0**)
and every symbol is resolved up front so a wrong/old dylib fails at load
with a precise "symbol missing (expected 1.19.x)" error, not mid-boot.

## 2. `krun_start_enter` semantics — the two things that shape the engine

Verbatim from the header doc:

> Starts and enters the microVM with the configured parameters. (…)
> This function consumes the configuration pointed by the context ID.
> (…) This function only returns if an error happens before starting the
> microVM. Otherwise, the VMM assumes it has full control of the
> process, and will call to exit() with the workload's exit code once
> the microVM shuts down.
> Error exit codes: 125 init cannot set up the environment;
> 126 init cannot execute; 127 init cannot find the executable.

Consequences (both implemented and documented in `vm.rs`):

1. **Exit code propagation is free** — no scratch disk needed (unlike
   the kvm engine). But `Vm::run()` NEVER RETURNS on success: the whole
   `myc` process exits with the container's code, exactly like
   `crun krun`. Fine for the CLI (`myc run --engine hvf-microvm …`);
   categorically unsuitable for embedding (myc-web) — acceptable today
   because the tier is `complete: false` and unreachable from `auto`.
   A future daemon-friendly variant would need a dedicated child
   process (fork-then-enter is risky in multithreaded embedders; a
   helper subcommand is the safe design).
2. **The guest runs in-process** (no child VMM). SIGKILL of `myc` is
   SIGKILL of the guest; cleanup on the error paths frees the ctx with
   `krun_free_ctx` (only valid BEFORE `krun_start_enter` — after it the
   context is consumed).

## 3. Kernel formats and the mycel assets

```c
#define KRUN_KERNEL_FORMAT_RAW 0        // x86 bzImage, arm64 raw Image
#define KRUN_KERNEL_FORMAT_ELF 1
#define KRUN_KERNEL_FORMAT_PE_GZ 2
#define KRUN_KERNEL_FORMAT_IMAGE_BZ2 3
#define KRUN_KERNEL_FORMAT_IMAGE_GZ 4
#define KRUN_KERNEL_FORMAT_IMAGE_ZSTD 5
```

- x86_64 Macs: asset `vmlinux-vmfs` (bzImage) → `RAW`. Solid.
- arm64 Macs: asset `vmlinux-vmfs-aarch64` (uncompressed `Image`) →
  `RAW` is our reading (an arm64 `Image` IS a raw image; libkrun's own
  examples load arm64 kernels as RAW). **UNCERTAIN-but-low-risk**: if
  the first boot rejects it, try `MYCEL_VM_KRUN_KERNEL_FORMAT=elf` with
  an ELF `vmlinux`, or check what libkrun's `KernelFormat::Raw` loader
  does with the arm64 image header on macOS/HVF.
- Override knob exists precisely for that first-boot debugging:
  `MYCEL_VM_KRUN_KERNEL_FORMAT=raw|elf|pe_gz|image_bz2|image_gz|image_zstd|<int>`.

### Guest-kernel config compatibility (read from
`bench/microvm/build_kernel_vmfs.sh`, both arches)

Present (needed by the libkrun container model): `VIRTIO_PCI` (+legacy),
`VIRTIO_MMIO` + `CMDLINE_DEVICES`, `VIRTIO_NET`, `VIRTIO_BLK`,
`VIRTIO_CONSOLE`, `VIRTIO_FS` + `FUSE_FS`, `EXT4_FS`, `DEVTMPFS`,
`BINFMT_SCRIPT`, `PROC/SYSFS/TMPFS`, arm64 `PL011` console
(`ttyAMA0`), x86 `SERIAL_8250_CONSOLE`.

Missing, and what it costs:

- `VIRTIO_VSOCK` is NOT built → **TSI networking impossible** with the
  published assets (libkrun's zero-config networking impersonates
  sockets over vsock; also `krun_set_port_map` is a TSI feature).
- No `CONFIG_NET_9P`, no `HW_RANDOM` — not needed by the model.

**Open kernel question for the first Mac session**: does libkrun's
built-in init need anything else we don't build (e.g. `FW_LOADER`?
`CRYPTO_USER_API`? random seed via virtio-rng?) — an allnoconfig kernel
is aggressive. If boot dies inside init, diff against libkrunfw's
known-good defconfig (libkrun's own kernel crate, same 6.12.x base) —
that is the reference "works with libkrun" config.

### Console

We pass NO `console=` (cmdline is `quiet loglevel=3`): libkrun wires its
implicit console itself (`krun_set_kernel_console` exists to override,
`krun_set_console_output` to redirect kernel output to a file; both
UNVALIDATED knobs). The workload's stdio rides the virtio-console
`krun-stdin/stdout/stderr` ports that `krun_start_enter` sets up when it
takes over the terminal — that is the documented container flow. Do NOT
call `krun_set_console_output` in this model unless you want the
workload's output in a file (it configures the implicit console device).

## 4. Rootfs options in v1.19.7 (why `krun_set_root` was chosen)

- `krun_set_root(ctx, dir)` — host DIRECTORY shared as guest root via
  in-process virtiofs. Zero copy, read-write, no daemon. Chosen: this is
  the kvm engine's "voie B" without virtiofsd.
- `krun_add_virtiofs{,2,3,4}` — additional shares (tag, shm/DAX size,
  read-only, semantics). **The built-in init mounts ONLY the root** —
  there is no API to tell the guest to mount an extra share at a
  container path, which is exactly why binds are refused in v1. (v2
  path: custom initramfs whose `/init` mounts `KRUN_FS_ROOT_TAG` +
  extra virtiofs tags, i.e. the kvm tier's `/myc/init-guest` recipe; the
  header explicitly notes `KRUN_FS_ROOT_TAG = "/dev/root"` for
  `krun_add_virtiofs3` root control, and `krun_disable_implicit_init`
  exists.)
- `krun_add_disk{,2,3}` / deprecated `krun_set_root_disk` /
  `krun_set_root_disk_remount` — block-device roots (raw/qcow2/vmdk,
  sync modes incl. macOS-specific `KRUN_SYNC_RELAXED`). Not used; a
  future "voie A equivalent" could `mkfs` an ext4 image and remount, but
  on macOS there is no mkfs.ext4 — virtiofs is the natural path.
- `krun_fs_add_overlay_file/_dir` — memory-backed virtual files injected
  into a virtiofs share (this is how `/init.krun` itself is injected;
  `krun_get_default_init` hands you the same binary). Potential v2 trick
  for `/etc/resolv.conf` or `/etc/hostname` injection WITHOUT an
  initramfs: overlay a file into the root share. UNCERTAIN: whether a
  hostname file has any effect (depends on the built-in init, which we
  cannot read from here — it lives inside the dylib).

**"libkrun URL" resolved**: the mission brief asked about a
"libkrun URL" config format (`krun_set_vm_path`?). **No such API exists
in v1.19.7** — configuration is setter-by-setter. What exists on the
`main` branch (libkrun 2.0, in flux) is composable `Payload` primitives
(`Payload::load_external(kernel, format, initrd, cmdline)`); and crun's
krun mode takes a VM config FILE with `kernel_path/kernel_format/
initrd_path/kernel_cmdline` fields (crun's own format, not libkrun's).
If a future libkrun 2.x is adopted, expect a migration to the
`krun_payload_*` API.

## 5. Networking on macOS (why v1 is offline)

macOS has no KVM and no tap devices. libkrun 1.19.x offers:

| Backend | Call | Notes |
|---|---|---|
| TSI (default if no net call) | — (implicit vsock) | guest sockets impersonated by the host; `krun_set_port_map(ctx, {"host:guest", …})` exposes guest ports on the host; NULL map = expose ALL listening ports. Needs guest `VIRTIO_VSOCK` — **our kernels don't build it** → unavailable today |
| userspace stream (passt-style) | `krun_add_net_unixstream(ctx, path\|fd, mac, features, flags)` | connect to a passt-compatible unix socket |
| userspace gram (gvproxy / vmnet-helper / socket_vmnet) | `krun_add_net_unixgram(ctx, path\|fd, mac, features, flags)` | `NET_FLAG_VFKIT` for gvproxy-vfkit; `NET_FLAG_DHCP_CLIENT` exists. This is the podman-machine-on-macOS shape |
| tap (Linux only) | `krun_add_net_tap(ctx, tap, mac, features, flags)` | the mission brief's "krun_set_net" — **Linux-only; corrected** |
| MAC-only | `krun_set_net_mac(ctx, mac)` | passt legacy |

v2 plan (documented, not wired): spawn `gvproxy` (or socket_vmnet) with a
port-forwarding config derived from the RunRequest, then
`krun_add_net_unixgram` to its socket. The mycel userland relay
(`portproxy.rs`, docker-proxy pattern) is reusable as a fallback for
loopback publishing once the guest has ANY routable address — it is
currently cfg(linux) only because nothing on macOS needs it yet.
Rootless note: gvproxy+vmnet.framework's "shared" mode needs no root;
the Apple `vmnet` framework entitlements apply to *vmnet* operation
modes, not to HVF itself.

## 6. Installing libkrun on a Mac (for the validator)

- Homebrew (no core formula as of the survey): `brew tap libkrun/krun &&
  brew install krunkit` — krunkit (the podman-machine macOS VMM) links
  `libkrun.dylib` under `/opt/homebrew/lib` (arm64) or `/usr/local/lib`
  (Intel). **UNCERTAIN whether the tap ships a standalone `libkrun`
  formula too — check `brew search libkrun` and prefer the explicit
  formula if present.**
- MacPorts: `sudo port install libkrun` → `/opt/local/lib/libkrun.dylib`.
- From source: `git clone https://github.com/libkrun/libkrun && make`
  (macOS builds default to HVF; `make` output is `libkrun.dylib`) —
  see the repo README for the Xcode command-line-tools prerequisite.
  Install path is wherever you put it; point `MYCEL_VM_LIBKRUN` at it.
- The engine searches, in order: `$MYCEL_VM_LIBKRUN`,
  `/opt/homebrew/lib/libkrun.dylib`, `/usr/local/lib/libkrun.dylib`,
  `/opt/local/lib/libkrun.dylib`.
- No runtime version query exists in the API (verified) — the engine
  just reports "expected 1.19.x" and fails loudly on missing symbols.

## 7. Hypervisor.framework entitlement (likely first-boot failure)

HVF (`hv_vm_create`) requires the calling process to be root OR the
binary to be signed with the `com.apple.vm.hypervisor entitlement`.
An unsigned/ad-hoc `myc` run as a normal user should therefore expect
`krun_start_enter` (or dlopen-time HVF init) to fail with `HV_DENIED` /
EPERM. Workarounds, in order of practicality:

```bash
sudo myc run --engine hvf-microvm alpine:3.20 -- echo hello   # root path
# or ad-hoc-sign the binary (no Apple developer account needed):
cat > myc-entitlements.plist <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.vm.hypervisor</key><true/>
</dict></plist>
EOF
codesign --force --sign - --entitlements myc-entitlements.plist "$(which myc)"
```

(Recipe from Apple's Hypervisor.framework documentation and common
krunkit/libkrun practice — **not executed here**, verify on hardware.)
krunkit ships with that entitlement — another reason the brew path is
the smooth start. Note: the DYLD environment must not be stripped for
entitlement-signed binaries — run `myc` directly, not through wrappers
that sanitize the environment.

## 8. Validation checklist (walk it on a Mac, in order)

Expected total: ~30 min on a networked arm64 Mac. Record every result in
this file's §10 (append, never rewrite history).

1. **Build**: `cargo build --release -p myc-cli` on the Mac (Xcode CLT
   installed). Sanity: `myc --version`.
2. **Doctor**: `MYCEL_VM_EXPERIMENTAL=1 myc doctor` — expect the
   hvf-microvm block: experimental guard ON, libkrow path found (after
   §6), guest kernel auto-downloaded (`~/.mycel/vm-assets/
   vmlinux-vmfs-aarch64`), plus the UNVALIDATED and entitlement rows.
   Without the env var: engine absent/unavailable.
3. **First boot** (the moment of truth):
   `MYCEL_VM_EXPERIMENTAL=1 myc run --engine hvf-microvm alpine:3.20 -- echo hello`
   - Prints `hello`, exit code 0 → the whole tier's thesis holds;
     proceed.
   - Exit 125/126/127 → libkrun init could not set up / exec: check the
     kernel-config gap (§3), try `MYCEL_VM_TRACE=1` for libkrun logs.
   - EPERM/HV_DENIED → §7 entitlement.
   - `-EINVAL` from `krun_set_kernel` → wrong kernel format (§3), try
     `MYCEL_VM_KRUN_KERNEL_FORMAT=elf`.
4. **Exit-code propagation**:
   `myc run --engine hvf-microvm alpine:3.20 -- sh -c 'exit 7'; echo $?`
   → must print 7.
5. **Env + workdir**: `-e MSG=ok alpine -- sh -c 'echo $MSG'` and
   `--workdir /tmp alpine -- pwd` (materialized image workdir too).
6. **Command override vs image CMD**: run an image with an entrypoint
   (redis:7-alpine `-- echo hi` must run echo, not redis-server).
7. **Refusals are clean**: `-v ./x:/x` and `-p 8080:80` must fail with
   the documented messages, not hang.
8. **Stdio takeover**: pipe big output (`seq 1 100000`) — no truncation,
   no CR pollution (virtio-console normalization is libkrun's job here;
   compare with the kvm tier's CRLF handling).
9. **Concurrent with native**: run one hvf container while a native
   `myc run` works — no interference (separate kernels by design).
10. **Then** (v2 candidates, each behind its own validation): gvproxy
    networking + `-p`; binds via custom initramfs; snapshot story (none
    in libkrun — warm pool); `codesign` distribution story.

## 9. Reproducing the type-check from Linux (how this was proven)

The whole tier was verified without Apple hardware like this:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
# zig provides the C cross-compiler that native build scripts
# (zstd-sys, ring, blake3) need even for `cargo check`:
curl -O https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz
tar xf zig-linux-x86_64-0.13.0.tar.xz && mv zig-linux-x86_64-0.13.0 ~/zig
# wrapper that rewrites cc-rs's `--target=arm64-apple-macosx` into zig's
# `aarch64-macos` (spelling differs!), then:
CC_aarch64-apple-darwin=<wrapper> AR_aarch64-apple-darwin="$HOME/zig/zig ar" \
  cargo check  --target aarch64-apple-darwin -p myc-backend
CC_aarch64-apple-darwin=<wrapper> AR_aarch64-apple-darwin="$HOME/zig/zig ar" \
  cargo clippy --target aarch64-apple-darwin -p myc-backend --lib --tests -- -D warnings
CC_aarch64-apple-darwin=<wrapper> AR_aarch64-apple-darwin="$HOME/zig/zig ar" \
  cargo clippy --target aarch64-apple-darwin -p myc-cli --bins -- -D warnings
```

Result at HEAD of `feat/macos-libkrun`: **zero errors, zero warnings**
on both `aarch64-apple-darwin` and `x86_64-apple-darwin` (lib+tests;
`examples/boot.rs` is kvm/Linux-only by design and excluded from Apple
targets). The pure planning logic runs under plain `cargo test -p
myc-backend` on Linux (module compiled under `cfg(test)`, the `wsl.rs`
precedent). NOTE: this proves COMPILATION for the Apple targets, nothing
more — linking a real binary needs Apple's linker (a Mac), and running
needs the hardware.

## 10. Env knobs (complete list, `vm.rs`)

| Variable | Default | Meaning |
|---|---|---|
| `MYCEL_VM_EXPERIMENTAL` | unset | `=1` required for the tier to be available at all |
| `MYCEL_VM_LIBKRUN` | search list | explicit `libkrun.dylib` path |
| `MYCEL_VM_KRUN_KERNEL` | `/usr/local/share/mycel/<asset>` | explicit kernel path |
| `MYCEL_VM_KRUN_KERNEL_FORMAT` | `raw` | `raw\|elf\|pe_gz\|image_bz2\|image_gz\|image_zstd\|<int>` |
| `MYCEL_VM_MEM` | `512M` | guest RAM (`512M`, `1G`, bare MiB) |
| `MYCEL_VM_CPUS` | `1` | vCPUs |
| `MYCEL_VM_TRACE` | unset | `=1` → libkrun log level 4 (debug) instead of 1 |
| `MYCEL_VM_ASSETS_DIR` / `MYCEL_VM_ASSETS_DIR`-family | `~/.mycel/vm-assets` | shared with the kvm engine (see `assets.rs`) |

## 11. Open uncertainties (honest list)

1. Whether the published vmfs kernels boot under libkrun/HVF at all
   (missing-config gap list in §3 is a guess from the container model's
   needs, not from a working boot).
2. arm64 `Image` + `KRUN_KERNEL_FORMAT_RAW` on HVF (§3).
3. libkrun built-in init behavior details: PATH handling (we resolve
   exec ourselves), env array application order, stdio wiring nuances,
   whether it needs virtio-rng.
4. Whether the Homebrew libkrun/krun tap ships `libkrun` itself or only
   krunkit (§6).
5. Exact HVF failure modes for an unsigned binary (§7 is best-effort
   documentation, not observation).
6. Console: we pass no `console=`; if kernel output matters during
   debugging, `krun_set_kernel_console` is the knob (untested).

## 12. Sources

- `include/libkrun.h` @ tag v1.19.7 (containers/libkrun) — all FFI
  signatures and doc quotes above.
- libkrun `docs/workloads.md` (main branch) — workload models
  (container-isolation vs UEFI-krunkit vs direct kernel boot), useful
  map of where 2.0 is going.
- Capsule M0 investigation, 2026-11-03 (`docs/plan/M0.md` §6 +
  `crates/capsule-vm/src/libkrun_ffi.rs`): v1.19.7 build, `krun_start_enter`
  in-process behavior verified on Linux/KVM, no snapshot API (verified
  against the header), kernel not bundled.
- Homebrew/libkrun install pointers: minikube krunkit driver docs,
  MacPorts port page (§6 links live in the repo READMEs).
