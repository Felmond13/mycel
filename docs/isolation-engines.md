# Isolation engines

Mycel never depends on a single isolation mechanism. Containers can run in
several "engines"; the choice is **automatic** — users never configure it —
and `myc doctor` prints the detected tiers so the choice is never a mystery.

| Tier | Engine (`myc doctor`) | Isolation | Status |
|---|---|---|---|
| Linux + KVM | `kvm-microvm` (Cloud Hypervisor driven by `myc-backend`) | **own kernel** — a container escape never shares the host kernel | prototype: boots the pinned-kernel workload (`bench/microvm`); arbitrary environments not wired through the guest yet |
| macOS | `hvf-microvm` (Virtualization.framework plan) | own kernel | architecture only (`docs/desktop-architecture.md`) |
| Windows | `wsl2-twin` (transparent proxy into WSL2) | containers execute in the Linux twin | shipped |
| Any Linux | `native-namespaces` (user namespaces, `myc-run`) | **shared kernel** — Docker-level, not better | default everywhere; carries the full feature set (binds, volumes, ports, pods) |

Selection policy: the default backend is the strongest **complete** tier —
today that is `native-namespaces` on Linux and `wsl2-twin` on Windows. The
microVM tiers become the default only when they carry the whole `RunRequest`
surface; until then `myc doctor` shows them as `PROTO` and `run()` on the
KVM engine refuses cleanly instead of pretending.

## Honest limitation of the fallback tier

`native-namespaces` shares the host kernel. That is exactly Docker's level
of isolation — not better. Whole-memory snapshot/restore is also impossible
there. The microVM tiers exist to lift precisely these two limits.

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

1. Guest integration for arbitrary environments: share the materialized
   rootfs into the VM (virtiofs), exec the manifest command inside, map
   `RunRequest` (binds → virtiofs shares, ports → tap/gvproxy). Then
   `kvm-microvm` becomes the Linux default and namespaces the fallback.
2. macOS tier via libkrun (KVM + Hypervisor.framework in one code path —
   the reason libkrun is the preferred vehicle long-term).
3. Snapshot/restore on the microVM tier: Firecracker path validated under
   WSL2 (91 ms median, live guest); CH re-validation on native KVM still
   pending (`bench/snapshot-native`); encrypted at rest.
4. Benchmarks reproduced per backend and reported separately (always vs
   Docker on the same machine).
