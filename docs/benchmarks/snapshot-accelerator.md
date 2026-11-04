# Snapshot accelerator (Firecracker) — measured results

Real measurement of the kvm-microvm **snapshot accelerator** introduced in
`feat/fc-accelerator` (`crates/myc-backend/src/firecracker.rs`): repeated
runs of the same environment restore a paused Firecracker microVM instead
of booting a kernel. Raw files (`cold.json`, `warm.json`, `ch.json`,
`env.txt`) are archived in `bench/snapshot-accel/results/` and never
edited after publication.

## What was measured

The full `myc run` path, end to end, same command, same image, same store:

```
myc run --engine kvm-microvm alpine:3.20 -- /bin/echo ok
```

Three series (hyperfine 2.0.0, n=30 each, every run verifies the output is
exactly `ok` and the exit code is 0 — a silently broken run fails the
bench):

| Series | Path taken by the engine |
|---|---|
| **cold** | snapshot cache wiped before EACH run (`--prepare`): materialize + `mkfs.ext4 -d` + Firecracker kernel boot + agent ready + pause + Full snapshot + command injection + resume |
| **warm** | snapshot present: Firecracker loads the snapshot (VM starts paused), host rewrites the scratch command block, resume, agent executes |
| **ch** (context) | the pre-existing fallback: Cloud Hypervisor voie B (virtiofs), `MYCEL_SNAPSHOT_CACHE=0` |

- Machine: 11th Gen Intel Core i5-1145G7 @ 2.60 GHz, 11 GiB RAM, WSL2
  (kernel 6.18.54.1-microsoft-standard-WSL2), Firecracker v1.17.0, guest
  kernel Linux 6.12.115 (vmfs build, ELF), 1 vCPU / 512 MiB, myc debug
  build. Run as a normal user (no published ports → no root needed).
  Everything captured in `bench/snapshot-accel/results/env.txt`.
- Mycel commit: `c2abf84` (`feat/fc-accelerator`).

## Results (n = 30 per series)

| Series | median | p95 | mean | σ |
|---|---|---|---|---|
| cold — FC boot + snapshot create | 1860.9 ms | 4406.6 ms | ~2.0 s | — |
| **warm — FC snapshot restore** | **171.5 ms** | **205.0 ms** | — | — |
| ch — Cloud Hypervisor voie B | 376.5 ms | 431.0 ms | 383 ms | 32 ms |

- **warm vs cold: ×10.9 median** (1689 ms saved per repeated run). The
  first run of an environment pays ~1.9 s (it also BUILDS the reusable
  snapshot); every later run of the same fingerprint costs ~172 ms. (The
  cold p95 of 4.4 s is one WSL2 outlier run; the cold median is the
  meaningful figure.)
- **warm vs the CH fallback: ×2.2 median** (205 ms saved) — and a much
  tighter distribution (warm p95 205 ms vs 447 ms measured for CH in the
  same campaign family).
- The restore measured at the VMM boundary alone (spawn → resumed) is
  **20–90 ms** — consistent with the `bench/snapshot-wsl2` kit that proved
  Firecracker restore is ALIVE under WSL2-KVM (91 ms median to first
  byte, 10/10 alive) where Cloud Hypervisor's restore is inert.

## Method notes (what makes these numbers honest)

- hyperfine `--prepare` (not `--setup`) wipes the snapshot cache before
  each cold run — with `--setup` only the first run is cold and the rest
  silently measure restores (caught during development of this bench).
- The measured command verifies output + exit code per run; the bench
  aborts on any failure (no `-i`/`--ignore-failure`).
- medians/p95 computed from the raw per-run wall times in the exported
  JSON (p95 = ceil(0.95·n)-th of the sorted times).
- The warm number includes the whole host-side path: myc startup, store
  open, fingerprint (asset hashes memoized — see below), lock, Firecracker
  spawn + snapshot load + resume, guest agent pickup, workload, teardown.

## Engineering findings measured along the way

- **The Firecracker vmstate embeds the drive paths** (spike, 2026-11-04):
  restored VMs re-open `rootfs.ext4`/`scratch.img` at the exact paths
  recorded at snapshot time — hence the stable per-fingerprint cache dir,
  with the rootfs mounted READ-ONLY in the guest under a tmpfs overlay so
  runs cannot dirty the shared image.
- **The command channel works through the guest page cache**: the agent
  runs `blockdev --flushbufs` before each poll of the scratch disk, so a
  host write while the VM is paused is picked up right after resume
  (validated in the spike, 3/3 restores with distinct commands + exit
  codes 3/42/0).
- **Firecracker has no ACPI S5**: `poweroff -f` leaves the VM halted
  forever; the agent uses `reboot -f` (Firecracker exits on guest reboot).
- **Exit-channel hardening**: the host writes a `MYCR` sentinel into the
  exit sector while the VM is paused, so a stale `MYCE…` marker from a
  previous run can never fake a success (one such transient was observed
  once before the sentinel; it would now surface as 125, loudly).
- **Environment images cannot be trusted for tools**: alpine ships no
  `blockdev` symlink — the agent uses a static busybox embedded by the
  host at `/myc/busybox`.
- **Fingerprint hashing is not free**: hashing the 20 MB kernel ELF +
  2.2 MB initramfs cost ~114 ms/run; digests are now memoized
  (path+size+mtime keyed), which took the warm median from 649 ms to
  184 ms (both measured, n=30).

## Reproduce

```bash
cargo build -p myc-cli
bash bench/snapshot-accel/run.sh            # n=30 per series, ~3 min
RUNS=50 bash bench/snapshot-accel/run.sh
```

Prerequisites: WSL2/Linux with a usable `/dev/kvm`, hyperfine, python3.
Assets auto-provision on the first run (Firecracker from the upstream
v1.17.0 release, kernel ELF from the local `build_kernel_vmfs.sh` tree).
See also `bench/snapshot-wsl2/README.md` for the VMM-level restore
measurement, and `docs/isolation-engines.md` for the isolation model.

## Limitations / what is deliberately NOT claimed

- **Binds are not served by the snapshot path** (`-v` needs virtiofs):
  such runs fall back to Cloud Hypervisor with one log line. Same for an
  explicit `MYCEL_VM_ROOTFS` (forces a CH voie).
- **One live VM per fingerprint** (the scratch file is per-run): a
  concurrent run of the same environment falls back to CH.
- The snapshot fixes the guest memory size and lease IP — both are part
  of the fingerprint (different `MYCEL_VM_MEM` or a different lease ⇒ a
  separate snapshot, cold-booted once).
- Cold runs are ~1.9 s: they materialize + mkfs + boot + snapshot. This
  is the one-time cost per fingerprint, not the steady state.
- `--net none` runs have no lease; published-port runs reuse the exact CH
  networking (bridge, lease, DNAT, portproxy, permanent ARP) — e2e
  `bench/microvm/e2e-vm.sh --with-ports` passes 27/27 with the
  accelerator active (redis served through a restored snapshot).
