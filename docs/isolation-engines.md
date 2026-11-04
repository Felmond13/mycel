# Isolation engines

Mycel never depends on a single isolation mechanism. Containers can run in
several "engines"; the choice is **automatic** — users never configure it —
and `myc doctor` prints the detected tiers so the choice is never a mystery.

| Tier | Engine (`myc doctor`) | Isolation | Status |
|---|---|---|---|
| Linux + KVM | `kvm-microvm` (Cloud Hypervisor driven by `myc-backend`) | **own kernel** — a container escape never shares the host kernel | **READY, opt-in** (`myc run --engine kvm-microvm`): arbitrary environments end to end — voie B virtiofs (production), voie A ext4 fallback; validated `bench/microvm/e2e-vm.sh` 27/27 |
| macOS | `hvf-microvm` (Virtualization.framework plan) | own kernel | architecture only (`docs/desktop-architecture.md`) |
| Windows | `wsl2-twin` (transparent proxy into WSL2) | containers execute inside the Linux twin | shipped |
| Any Linux | `native-namespaces` (user namespaces, `myc-run`) | **shared kernel** — Docker-level, not better | default everywhere; carries the full feature set (binds, volumes, ports, pods) |

Selection policy: the default backend is the strongest **complete** tier —
today that is `native-namespaces` on Linux and `wsl2-twin` on Windows.
`kvm-microvm` is now complete too (`READY` in `myc doctor`, full
`RunRequest` surface), but stays opt-in via `--engine kvm-microvm` until it
gets more soak time; the automatic choice keeps pointing at namespaces, and
`myc doctor` explains what each tier does not give you.

## Honest limitation of the fallback tier

`native-namespaces` shares the host kernel. That is exactly Docker's level
of isolation — not better. Whole-memory snapshot/restore is also impossible
there. The microVM tiers exist to lift precisely these two limits.

## The `kvm-microvm` engine (real execution)

```bash
myc run --engine kvm-microvm alpine:3.20 -- echo hello     # sortie hello, code 0
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

Network: a VM cannot share the host network namespace, so "host" mode is
emulated — every port the image exposes (or `-p`) is published. External
clients go through `nat PREROUTING` DNAT + FORWARD rules tagged `myc-<n>`
(inserted at the top of FORWARD: Docker's DROP policy shadows appended
rules — measured). `127.0.0.1` clients are served by a userland TCP relay
(`portproxy.rs`, the docker-proxy pattern) because WSL2's kernel never
applies `nat OUTPUT` DNAT to loopback-sourced SYNs (verified with
tcpdump). Tap/bridge/iptables need root — exactly like the Docker daemon;
runs without ports need no privileges at all.

Validation (`bench/microvm/e2e-vm.sh`, replayable, delta-based residue
assertions): **27/27 PASS** — echo/env/bind ro (+ write refused)/exit
7/exit 42/workdir/hostname/voie A/redis `-p 6399:6379` +PONG from
`127.0.0.1`, and zero residue after every single run (VMM, virtiofsd,
taps, DNAT rules, ARP entries, state dirs; `br-myc` persists by design,
docker0-style). Latency e2e `alpine echo` (hyperfine n=10, 2 warmups,
`bench/microvm/latency-vm.sh`, results archived in `results/latency-vm.*`):
voie B **351 ms** mean (σ 30), voie A **285 ms** (σ 94, incl. 69 ms mkfs),
native namespaces **77 ms** — the VM tier costs ~4× today, mostly kernel
boot + virtiofs handshake; the warm pool (§ below) is the answer for
latency-critical paths.

Known limits (v1): `--map-user` ignored (workloads run as guest root);
x86_64 guests only; one /24 bridge (`MYCEL_VM_CIDR`, 253 concurrent
networked guests); `--join-net`/`--lazy`/`--run-dir` refused on this
engine; PID 1 does not reap orphaned grandchildren; a SIGKILLed `myc`
bypasses cleanup (SIGINT/SIGTERM are handled and tear everything down).

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
| Any platform (fallback) | warm pool (`bench/warm-pool`) | **measured**: handoff **28 ms median / 61 ms p95** vs cold boot **299 ms median** (n=30 each, same harness, slim kernel) — ×10.7; works everywhere (processes + taps only, no vCPU save/restore); limits: 1 ready guest per resident VM, refill = background cold boot, N × 512 MiB resident |
| macOS (HVF) | plan | libkrun has no snapshot API → warm pool as stopgap; Hypervisor.framework suspend/resume to be investigated |

Bottom line: the Windows path exists — **Firecracker snapshot/restore
under WSL2 lands sub-200 ms with a live guest** — and the **warm pool** is
the everywhere-fallback (already ×10.7 over cold boot on the same
machine). Raw measurement files live in each bench `results/` directory
(never edited after publication).

## Roadmap

1. ~~Guest integration for arbitrary environments~~ **done** (voie B
   virtiofs + voie A ext4, e2e 27/27): remaining before `kvm-microvm`
   becomes the Linux default — soak time, PID-1 reaping, map_user, and
   reproducing the e2e suite on native KVM.
2. macOS tier via libkrun (KVM + Hypervisor.framework in one code path —
   the reason libkrun is the preferred vehicle long-term).
3. Snapshot/restore on the microVM tier: Firecracker path validated under
   WSL2 (91 ms median, live guest); CH re-validation on native KVM still
   pending (`bench/snapshot-native`); encrypted at rest.
4. Benchmarks reproduced per backend and reported separately (always vs
   Docker on the same machine).
