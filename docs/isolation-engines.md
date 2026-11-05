# Isolation engines

Mycel never depends on a single isolation mechanism. Containers can run in
several "engines"; the choice is **automatic** — users never configure it —
and `myc doctor` prints the detected tiers so the choice is never a mystery.

| Tier | Engine (`myc doctor`) | Isolation | Status |
|---|---|---|---|
| Linux + KVM | `kvm-microvm` (Cloud Hypervisor driven by `myc-backend`) | **own kernel** — a container escape never shares the host kernel | **READY — Linux default**: `myc run` picks it automatically whenever it is complete and the request is compatible (see the auto policy below); validated `bench/microvm/e2e-vm.sh` 27/27 |
| macOS | `hvf-microvm` (libkrun over Hypervisor.framework, `myc-backend/src/vm.rs`) | **own kernel** — a container escape never shares the host kernel | **EXPERIMENTAL — implemented + type-checked, UNVALIDATED on Apple hardware**: requires `MYCEL_VM_EXPERIMENTAL=1` + `libkrun.dylib`; network-less v1 (section below, `bench/macos-libkrun/NOTES.md`) |
| Windows | `wsl2-twin` (transparent proxy into WSL2) | containers execute inside the Linux twin | shipped |
| Any Linux | `native-namespaces` (user namespaces, `myc-run`) | **shared kernel** — Docker-level, not better | the fallback (and the only tier for pods, interactive runs and pasta isolation); carries the full feature set (binds, volumes, ports, pods) |

## Automatic selection (the `auto` policy)

The choice is automatic — users never configure it. `myc run` with no
`--engine` (or `--engine auto`) takes the strongest **complete** engine
that can carry the exact request; the policy lives in ONE pure function
(`myc_backend::decide_auto_engine`) shared by the CLI and the web
dashboard, so a container started from the terminal and one started from
the browser get the same treatment:

| request | `auto` picks |
|---|---|
| simple run, kvm READY on this machine | `kvm-microvm` — stderr: `engine: kvm-microvm (auto; strongest available)` |
| `--join-net` (stack services in pod mode) | native — pods are namespaces by construction |
| `--run-dir` (web UI lifecycle contract) | native — the caller owns a host directory |
| interactive run (no command + a terminal) | native — the VM console is output-only; `myc shell` forces native for the same reason |
| `--net isolated` (pasta) | native — that isolation IS the namespaces feature |
| publishes ports (`-p` / image ports) without root | native — the VM bridge/tap/DNAT need a root myc |
| kvm not complete on this machine | native, quietly — `myc doctor` lists exactly what is missing |

Every non-quiet fallback narrates itself on stderr
(`engine: native-namespaces (auto; …)`); an explicit
`--engine kvm-microvm` / `--engine native-namespaces` bypasses the policy
and never narrates. Stacks (`myc up`, web stacks) run on namespaces on
purpose: their services reach each other over the host network or the
pod's private one — a per-service microVM would isolate them from each
other. Readiness ("complete") counts guest assets present in the
`~/.mycel/vm-assets/` download cache as ready; a fully missing asset
still self-downloads on an *explicit* `--engine kvm-microvm` run, but the
automatic default never depends on the network.

## Honest limitation of the fallback tier

`native-namespaces` shares the host kernel. That is exactly Docker's level
of isolation — not better. Whole-memory snapshot/restore is also impossible
there. The microVM tiers exist to lift precisely these two limits.

## The `kvm-microvm` engine (real execution)

```bash
myc run alpine:3.20 -- echo hello                            # auto — takes the microVM when READY
myc run --engine kvm-microvm alpine:3.20 -- echo hello       # same, forced (never narrates)
myc run --engine kvm-microvm -e MSG=ok alpine -- sh -c 'echo $MSG'
myc run --engine kvm-microvm -v ./data:/data:ro alpine -- cat /data/f.txt
sudo myc run --engine kvm-microvm -p 6399:6379 redis:7-alpine   # 127.0.0.1:6399 → +PONG
```

The whole `RunRequest` surface works: manifest command (entrypoint + cmd,
`--` override), env vars (image ENV, then `-e`), bind mounts, workdir,
hostname, published ports, exit-code propagation (`sh -c "exit 7"` returns
7), keep-rootfs. How the rootfs enters the guest — two "voies":

- **Voie B — virtiofs (production path)**: [virtiofsd] shares the
  materialized rootfs directory; the guest mounts `mycroot` and
  `switch_root`s. Zero copy, bind mounts become extra shares (ro enforced
  by both a read-only daemon and a read-only guest mount). Requires the
  `virtiofsd` package; without it the engine falls back to voie A and
  `myc doctor` reports PARTIAL.
- **Voie A — ext4 (guaranteed fallback)**: `mkfs.ext4 -d <rootfs>` (69 ms
  for alpine, measured) builds a per-run image mounted by the guest. One
  copy, read-write guest root; binds are not supported (refused with a
  clear error). Force either voie with `MYCEL_VM_ROOTFS=virtiofs|ext4`.

Guest contract (assets in `bench/microvm`): kernel
`build_kernel_vmfs.sh` → `vmlinux-vmfs` (allnoconfig + `VIRTIO_FS`,
`EXT4_FS`, `FUSE_FS`, `OVERLAY_FS` — the slim bench kernel has none of
those), initramfs `mkinit-vm.sh` → generic stage 1 (mount rootfs, create
`/proc /sys /dev /tmp /run` — OCI images ship none of them — then
`switch_root`). Stage 2 is `/myc/init-guest`, written by the host into the
rootfs with `/myc/run.env` (strictly shell-quoted command/env/binds; a
`run.json` twin is kept for humans). The command runs as a child of the
guest's PID 1, its console is streamed live to the host stdout (CRLF
normalized), and its exit code is written as raw bytes at offset 0 of a
scratch virtio-blk disk — the host reads it right after the VMM exits, so
propagation works on both voies with no filesystem needed.

Distribution: `vmlinux-vmfs` + `init-vm.cpio` ship as GitHub release
assets — `install.sh --with-vm` pre-installs them into
`/usr/local/share/mycel/`, and the engine auto-downloads a missing asset
into `~/.mycel/vm-assets/` on first use (sha256-checked when the release
ships sidecars); `cloud-hypervisor`/`virtiofsd` remain distro packages.

aarch64 (pipeline status, no unmeasured claims): releases also ship
`myc-linux-aarch64` (native build on GitHub's free arm64 runners,
continuously validated by `.github/workflows/ci-arm64.yml` — fmt + clippy
+ the whole test suite) and the aarch64 guest assets
`vmlinux-vmfs-aarch64` + `init-vm-aarch64.cpio` (same scripts with
`ARCH=aarch64`, native arm64 build; PL011 console). These assets are
**published, not validated**: the engine still executes x86_64 guests
only — on an aarch64 host it reports itself unavailable (`myc doctor`
says exactly why) and workloads run through `native-namespaces`, the
default. Engine wiring on arm64 is pending hardware validation; no boot,
e2e or latency number exists for aarch64, so none is claimed.

Network: a VM cannot share the host network namespace, so "host" mode is
emulated — every port the image exposes (or `-p`) is published. External
clients go through `nat PREROUTING` DNAT + FORWARD rules tagged `myc-<n>`
(inserted at the top of FORWARD: Docker's DROP policy shadows appended
rules — measured). `127.0.0.1` clients are served by a userland TCP relay
(`portproxy.rs`, the docker-proxy pattern) because WSL2's kernel never
applies `nat OUTPUT` DNAT to loopback-sourced SYNs (verified with
tcpdump). Tap/bridge/iptables need root — exactly like the Docker daemon;
runs without ports need no privileges at all. Root runs are networked
even without published ports: the guest gets a default route (kernel
cmdline `mycgw=`), the host forwards and masquerades the bridge /24 out
of the default-route interface (tagged `myc-egress` rules; the FORWARD
pair matters because Docker's DROP policy shadows nothing inserted after
it — measured), and `/etc/resolv.conf` is rewritten per run from
`MYC_DNS` (host resolvers, `MYCEL_VM_DNS` to override). Measured inside
the VM: `apk add curl` + HTTPS 200 from a snapshot-restored guest.
Non-root runs without ports stay offline (bridge/tap need root — same
line as the Docker daemon).

Validation (`bench/microvm/e2e-vm.sh`, replayable, delta-based residue
assertions): **27/27 PASS** — echo/env/bind ro (+ write refused)/exit
7/exit 42/workdir/hostname/voie A/redis `-p 6399:6379` +PONG from
`127.0.0.1`, and zero residue after every single run (VMM, virtiofsd,
taps, DNAT rules, ARP entries, state dirs; `br-myc` persists by design,
docker0-style). Latency e2e `alpine echo` (hyperfine n=10, 2 warmups,
`bench/microvm/latency-vm.sh`, results archived in `results/latency-vm.*`):
voie B **351 ms** mean (σ 30), voie A **285 ms** (σ 94, incl. 69 ms mkfs),
native namespaces **77 ms** — the VM tier costs ~4× today, mostly kernel
boot + virtiofs handshake; the snapshot accelerator + early exit-at-marker
(§ below, and `docs/benchmarks/warm-pool-verdict.md`) is the answer for
latency-critical paths — a warm VM pool was measured and ruled out there.

Known limits (v1): `--map-user` ignored (workloads run as guest root);
x86_64 guests only (aarch64: binary + guest assets published, engine
wiring pending hardware validation — see the distribution note); one /24 bridge (`MYCEL_VM_CIDR`, 253 concurrent
networked guests); non-root no-port runs boot offline (root runs get
egress NAT — see above; `auto` still keeps pasta-isolated and
rootless-port-publishing runs native);
`--join-net`/`--lazy`/`--run-dir` refused on this engine (the auto policy
falls back to namespaces for them, with the reason on stderr); PID 1 does
not reap orphaned grandchildren; a SIGKILLed `myc` bypasses cleanup
(SIGINT/SIGTERM are handled and tear everything down).

## The `hvf-microvm` engine (macOS, EXPERIMENTAL)

The macOS tier is real code now (`crates/myc-backend/src/vm.rs`), written
against libkrun v1.19.x — the same VMM library `crun --krun` embeds, and
the long-term vehicle of choice because one library covers KVM on Linux
and Hypervisor.framework on macOS. It is also the project's most
honest tier: **nothing has been executed on Apple hardware** (the whole
tier was written and checked from a Windows/WSL2 workstation), so the
probe never claims more than "present + opt-in", `complete` stays false,
and the automatic `auto` policy can therefore never pick it. Explicit
use only:

```bash
brew tap libkrun/krun && brew install krunkit   # ships libkrun.dylib (or: sudo port install libkrun)
export MYCEL_VM_EXPERIMENTAL=1
myc doctor                                      # per-check honesty, including UNVALIDATED
myc run --engine hvf-microvm alpine:3.20 -- echo hello
```

Design (mirrors `crun krun`, the reference embedder):

- **Kernel**: direct kernel boot with the SAME published release assets
  as the kvm engine — `vmlinux-vmfs-aarch64` on arm64 Macs, `vmlinux-vmfs`
  on x86_64 (`krun_set_kernel`, format RAW; auto-provisioned from
  `~/.mycel/vm-assets/` exactly like the Linux tier). No kernel ships
  inside libkrun 1.19.x.
- **Rootfs**: the materialized rootfs directory is shared as the guest
  root over **virtiofs in-process** (`krun_set_root`) — zero copy, no
  host daemon (no virtiofsd needed; libkrun implements the device
  itself). This is the kvm engine's "voie B", natively.
- **Init**: libkrun injects its own built-in init (`/init.krun`) into the
  root share — no initramfs in this model.
- **Workload**: `krun_set_exec(path, argv, envp)` + `krun_set_workdir`;
  stdio is taken over by `krun_start_enter`, which wires the workload's
  stdin/stdout/stderr to the host terminal (virtio-console ports).
- **Exit code**: `krun_start_enter` runs the guest IN-PROCESS and calls
  `exit()` with the workload's exit code when the guest powers off — on
  success `Vm::run` never returns; the `myc` process itself exits with
  the container's code (125/126/127 are libkrun's init error codes).
  This is precisely `crun krun`'s behavior.

What works by construction (but is UNVALIDATED): the whole
RunRequest→libkrun mapping — command override, env layering
(defaults → image ENV → `-e`), workdir, exec resolution against the
guest PATH, guest RAM/vCPUs (`MYCEL_VM_MEM`, `MYCEL_VM_CPUS`).

What is refused, with actionable errors (never silently dropped):
bind mounts (libkrun 1.19.x has no guest-mount API for extra virtiofs
shares — carrying them needs a custom initramfs, like the kvm tier's
`/myc/init-guest`); published ports (the tier is network-less in v1).
Ignored and documented: hostname, `--map-user` (no libkrun API for UTS
namespaces or uid maps).

Networking (design, not wired): macOS has no `/dev/kvm` and no tap —
libkrun's macOS backends are `krun_add_net_unixstream` (passt-style) and
`krun_add_net_unixgram` (gvproxy / socket_vmnet); TSI (vsock socket
impersonation, zero-config host networking) needs a guest kernel with
`VIRTIO_VSOCK`, which the published vmfs kernels do not build in. The
gvproxy path is the documented plan for v2 (see NOTES.md).

Guards (structural honesty):

- `probe()`/`run()` gate on ALL of: `libkrun.dylib` found
  (`MYCEL_VM_LIBKRUN` override; Homebrew arm64/Intel + MacPorts search
  paths), guest kernel present, and `MYCEL_VM_EXPERIMENTAL=1` — one
  actionable error per missing factor.
- `myc doctor` shows an explicit "hardware validation: UNVALIDATED" row
  and a Hypervisor.framework entitlement note (HVF needs root or a
  binary signed with `com.apple.vm.hypervisor` — a likely first-boot
  failure mode on an unsigned binary).
- The pure planning logic (`plan_run`, availability, exec resolution,
  config parsing) is platform-independent and unit-tested on Linux;
  the macOS code is type-checked (and clippy-clean) for BOTH
  `aarch64-apple-darwin` and `x86_64-apple-darwin` from the Linux CI
  host via a zig-cc cross toolchain.

## Hardening the Firecracker path: the official jailer (`MYCEL_FC_JAILER=1`)

The snapshot accelerator boots Firecracker as a direct child of the engine —
parity with the (unjailed) Cloud Hypervisor path. The upstream jailer of the
same v1.17.0 release adds the missing production isolation, opt-in via
`MYCEL_FC_JAILER=1` (**default OFF** until more exposure; see the measured
verdict below):

- **chroot + pivot_root** into a per-run tree
  `<snapshots_root>/jails/firecracker/<run-id>/root` (the v1.17 layout), in a
  private **mount namespace**;
- **uid/gid drop** to a dedicated account — uid/gid **20000:20000**, below
  every login-uid range (Linux assigns users from 1000), so the VMM matches
  no real user; `CapEff`/`CapPrm` end at zero and `NoNewPrivs` is set;
- **device nodes re-created inside the jail** (`/dev/kvm`, `/dev/net/tun`,
  `/dev/urandom`, `/dev/userfaultfd`) owned by that account — the host
  `/dev/kvm` (0660 root:kvm) is never touched;
- **seccomp**: Firecracker's own filter (it applies it jailed or not —
  `Seccomp: 2` in `/proc/<pid>/status` either way; the jailer adds the
  chroot/uid layers on top).

Mechanics worth knowing (all measured on the WSL2 dev machine, proofs in
`docs/benchmarks/fc-jailer.md`): the jailer v1.17 `execve`s the VMM when run
without `--daemonize`/`--new-pid-ns`, so the spawned pid IS Firecracker —
SIGTERM/exit-code/cleanup semantics are unchanged. Everything the VM must
see (kernel ELF, initramfs, rootfs, scratch, vmstate, mem) is hard-linked
into `<jail>/root/run/` at stable `/run/<name>` paths before launch (the
Firecracker vmstate re-opens drives by the paths the VM saw — hence a
`jailer=1` epoch in the snapshot fingerprint; jailed and direct snapshots
are separate caches). Hard links are instant even for the 512 MiB `mem.img`;
a cross-device snapshot root falls back to a copy. The jail lives under the
snapshots root, never `/tmp`: tmpfs is typically mounted `nodev`, and a
device node that cannot be opened kills the jailed VMM with `EACCES` for
everyone, root included (measured — that is not a jailer bug, it is a mount
flag). Networking stays host-managed: v1.17 has no `--net-tap` anymore (it
moved tap creation entirely to the caller) and mycel passes no `--netns`,
so the VMM opens the host tap from the host network namespace — the tap is
created owned by uid 20000 (`ip tuntap ... user 20000`) because TUNSETIFF
from a capability-less process only accepts the device's owner (any other
uid: EPERM, measured). Cgroups: the jailer only touches them when `--cgroup`
limits are passed; mycel passes none — WSL2 exposes cgroup v2 without
delegation, and the CH path enforces no limits either (parity; adding
`--cgroup` knobs later is trivial). Root is required (chroot, mknod, tap
ownership); a non-root jailed request falls back to Cloud Hypervisor with
the reason on stderr, never to un-jailed Firecracker.

Measured verdict (WSL2 dev machine, hyperfine n=30 per scenario, every run
verified `ok`+exit 0; full methodology + proofs: `docs/benchmarks/fc-jailer.md`):
warm restore **jailed 224.2 ms median / 268.8 ms p95 vs direct 243.9 ms /
344.5 ms p95** — the jailer costs nothing measurable (the jailed median is
even 19.7 ms lower, inside run-to-run σ; the jailed p95 is 76 ms BETTER),
while adding chroot, uid drop, zero capabilities and a private mount
namespace. Cold boot jailed: 1.72 s median (snapshot creation included).
Recommendation: keep the default OFF for one more cycle of exposure, then
flip it — it is strictly more isolation for no measurable latency.

## microVM boot benchmark (`bench/microvm`)

Same static musl binary (`benchhttp.rs`, std-only) everywhere: a `FROM
scratch` Docker container vs a KVM microVM booted by
`myc-backend/examples/boot` (pinned Linux 6.12.x, virtio built in, busybox
init, static-IP guest on a host tap — no userland proxy in Mycel's path).

Network (kvm-microvm): docker0 pattern — each VM gets its own tap
`myc-<n>` on the persistent bridge `br-myc` (which holds the subnet host
IP) with a sequential lease, first free from `.2` — deterministic, bench
scripts curling `192.168.77.2` keep working. N concurrent VMs per /24
(`MYCEL_VM_CIDR`); published ports are DNAT'd per lease, rules tagged
`myc-<n>`.

Protocol: T0 = command launch → T1 = first byte of a `200` received by the
same client loop; 5 unmeasured warmups then 30 measured runs per series,
ABBA order (A=docker, C=microvm, C, A), medians + p95 from the raw
nanosecond `.times` files, `hyperfine` as orchestrator; every run fails if
it leaks anything (container, tap, VMM process). Run as root (the KVM tap
requires it — exactly like the Docker daemon of the baseline):

```bash
cd bench/microvm
./build_kernel.sh          # once (Linux 6.12.x, virtio built in)
./mkinitramfs.sh           # compiles benchhttp (rustc, static musl)
./mkimage.sh               # docker build benchhttp:microvm
sudo ./run.sh              # campaign: results/*.times + *.json + env.txt
```

Results live in `docs/benchmarks/microvm-boot.md` (raw files archived,
never edited after publication).

## Snapshot / restore status (measured, not assumed)

- **libkrun v1.19.7**: no snapshot/restore API at all (verified against
  `include/libkrun.h`, 2026-11-03).
- **Cloud Hypervisor v54.0** under **WSL2's KVM**: snapshot works (~0.3 s
  for 512 MiB), restore loads memory and runs the vCPU — but the guest
  stays inert (no timer, no interrupts; all three memory-restore modes
  tested). WSL2's KVM does not preserve the vCPU/MSR state CH relies on
  across save/restore. Evidence: Capsule M0 investigation, 2026-11-03.
- **Firecracker v1.17.0** under the **same WSL2 KVM**: snapshot/restore
  **works with a live guest** (2026-11-04, `bench/snapshot-wsl2`). Same
  host, same inert-guest detector — Firecracker's save/restore path is
  distinct from CH's and survives WSL2's KVM.

### Multiplatform snapshot matrix (2026-11-04)

| Platform | Path | Status (measured) |
|---|---|---|
| Linux, native KVM | CH v54 snapshot/restore | kit ready (`bench/snapshot-native`) — **to be executed on native KVM**; the WSL2 run aborted inert, which is a WSL2-KVM artifact, not a verdict on native KVM |
| Windows (WSL2) | Firecracker v1.17 snapshot/restore | **viable, measured** (`bench/snapshot-wsl2`): boot 389 ms median; snapshot create 614 ms (512 MiB); restore **10/10 alive, 91 ms median, 161 ms p95** (T0 process launch → first byte of `200`, resume included; liveness = console heartbeat + HTTP) |
| Windows (WSL2) | CH v54 restore | inert (0 interrupts; 3 memory modes) — dead end on WSL2-KVM |
| Any platform (fallback) | warm pool (`bench/warm-pool`) | prototype **measured** (handoff 28 ms median vs cold 299 ms, ×10.7) but **ruled out on the production path** (2026-11-05): with the FC snapshot accelerator shipped, a pre-booted VM eliminates only ~21 ms of the ~225 ms e2e (~9 %) — the rest of its theoretical ceiling is host-side cost recovered by product fixes instead. Full campaign: `docs/benchmarks/warm-pool-verdict.md`; the kit remains the documented fallback for restore-less platforms |
| macOS (HVF) | libkrun | engine implemented (EXPERIMENTAL); no snapshot API in libkrun → warm pool as stopgap; Hypervisor.framework suspend/resume to be investigated |

Bottom line: the Windows path exists — **Firecracker snapshot/restore
under WSL2 lands sub-200 ms with a live guest** — and the warm pool
stays what it always was: the everywhere-**fallback** for platforms
without vCPU restore, not a product feature on the snapshot path
(measured, `docs/benchmarks/warm-pool-verdict.md`). Raw measurement
files live in each bench `results/` directory (never edited after
publication).

## Roadmap

1. ~~Guest integration for arbitrary environments~~ **done** (voie B
   virtiofs + voie A ext4, e2e 27/27). ~~`kvm-microvm` becomes the Linux
   default~~ **done** — the `auto` policy picks it whenever READY and the
   request is compatible (§ Automatic selection). Remaining hardening:
   PID-1 reaping, `--map-user`, and reproducing the e2e suite on native
   KVM. ~~Guest egress NAT~~ **done** — root runs are networked without
   published ports (default route + MASQUERADE + per-run DNS; measured
   with `apk add curl` from a snapshot-restored guest).
2. ~~macOS tier via libkrun~~ **engine written** (`hvf-microvm`,
   EXPERIMENTAL — see the section above): FFI, planning, probe and CLI
   wiring exist and are type-checked for both Apple targets; what
   remains is execution on real hardware (first boot, e2e suite, then
   networking via gvproxy, binds via a custom initramfs, and per-backend
   benchmarks — no number exists yet).
3. Snapshot/restore on the microVM tier: Firecracker path validated under
   WSL2 (91 ms median, live guest); CH re-validation on native KVM still
   pending (`bench/snapshot-native`); encrypted at rest.
4. Benchmarks reproduced per backend and reported separately (always vs
   Docker on the same machine).
