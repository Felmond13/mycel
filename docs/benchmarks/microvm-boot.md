# microVM boot vs Docker — measured results

First real measurement of the `kvm-microvm` engine (prototype) against a
Docker container, **same static workload binary on both sides**. Raw files
(`*.times`, hyperfine `*.json`, `env.txt`) are archived next to this report
and never edited after publication.

## Setup (all captured in `env.txt`)

- Machine: 11th Gen Intel Core i5-1145G7 @ 2.60 GHz, 4C/8T, 11 GiB RAM,
  WSL2 (kernel 6.18.54.1-microsoft-standard-WSL2, Hyper-V), on AC power.
- Docker 29.1.3 (Ubuntu package, systemd daemon, warm) — baseline `A`.
- Mycel `kvm-microvm` — Cloud Hypervisor **v54.0**, guest kernel **Linux
  6.12.115** (virtio built-in, `bench/microvm/build_kernel.sh`,
  sha256 `92684523…`), 1 vCPU / 512 MiB, busybox initramfs with the
  workload as PID 1 (sha256 `0f5bfa00…`) — scenario `C`.
- Workload: `benchhttp.rs`, one static-musl binary (sha256 `6cf7c541…`)
  in the scratch Docker image **and** in the initramfs. Serves `200 ok`;
  200 ms console heartbeat.
- Metric: T0 (command launch) → T1 (first byte of a `200` received by the
  same client loop). 5 unmeasured warmups + 30 measured runs per series,
  ABBA order (A, C, C, A); every run fails on any leak (container, tap,
  VMM process). Orchestrator: hyperfine 2.0.0; medians/p95 from raw
  nanosecond `.times` files. Run as root (the KVM tap needs CAP_NET_ADMIN,
  exactly like the Docker daemon of the baseline).
- Mycel commit: `505b833`.

## Results (n = 30 per series)

| Series | median | p95 | mean | CV |
|---|---|---|---|---|
| A — docker (1st half) | 469.2 ms | 1354.1 ms | 674.3 ms | 63.1 % |
| A_b — docker (2nd half) | **375.5 ms** | **447.1 ms** | 383.7 ms | 5.9 % |
| C — microvm (1st half) | 416.0 ms | 1056.3 ms | 475.2 ms | 34.8 % |
| C_b — microvm (2nd half) | 428.2 ms | 1074.4 ms | 507.7 ms | 36.4 % |

## Honest analysis

- **Boot a full Linux kernel in a KVM VM ≈ as fast as Docker's container
  create+start** on this machine: microvm medians 416–428 ms vs Docker's
  clean-series median 375 ms — a **+12 %** gap that buys a dedicated guest
  kernel (no shared-kernel isolation).
- **The ABBA design caught a real artifact**: Docker's first half (CV 63 %,
  p95 1.35 s) was disturbed — the WSL instance had been restarted just
  before the campaign and the daemon was still settling; the second half is
  clean (CV 5.9 %). Per protocol, both halves are published; the comparison
  uses the clean Docker series against the microvm series, which shows **no
  drift** between its own halves (416 → 428 ms, ~3 %).
- microvm p95 (~1.06 s) trails Docker's (~0.45 s): tail runs pay KVM
  vCPU/MSR setup under WSL2's nested virtualization. This is the number to
  attack next (pinned vCPU affinity, `--platform` tweaks), and the reason
  to re-measure on native KVM where the tax should shrink.
- Known WSL2-specific findings from the same investigation (see
  `docs/isolation-engines.md`): snapshot/restore leaves the guest inert
  under WSL2-KVM, and residual taps holding the subnet IP silently break
  routing (the engine now clears its tap before every launch).

## Campaign v2 — after the ARP fix (commit `cdbf431`)

The tail analysis (`microvm-tail-analysis.md`) proved the v1 p95 plateau
(~1060 ms) was the kernel's INCOMPLETE-neighbour ARP timer on first
contact, not KVM scheduling. Fix: the engine now pre-seeds a permanent ARP
entry for the (fixed) guest MAC right after tap creation. Re-measured,
same protocol (ABBA, n=30/series):

| Series | median | p95 | CV |
|---|---|---|---|
| A — docker | 455.4 ms | 512.5 ms | 8.5 % |
| A_b — docker | 479.8 ms | 513.2 ms | 4.0 % |
| C — microvm | **404.0 ms** | **624.5 ms** | 17.5 % |
| C_b — microvm | **409.7 ms** | 9044.9 ms* | 216 %* |

**The microVM now beats Docker on the median (~405 vs ~455-480 ms) with a
full dedicated kernel.** 58 of 60 microvm runs finished in 379-548 ms
(C: 30/30 under 707 ms, zero plateau).

*Honest asterisk: exactly 2 of 60 runs took ~9 045 ms (both in C_b,
binary — 9044 and 9052 ms, suspiciously identical). Working theory: the
first TCP SYN leaves while the guest has not brought eth0 up yet; with the
ARP entry permanent, the SYN is silently dropped and Linux's SYN
retransmission backoff (~1+2+4 s…) delays the connection until the guest
is actually up. Candidate remedies (not yet applied): brief high-frequency
probing before first SYN, or seeding the ARP entry only after a link-local
signal from the guest. Raw files archived as `results-v2/`.

## Reproduce

```bash
cd bench/microvm
./build_kernel.sh && ./mkinitramfs.sh && ./mkimage.sh
sudo ./run.sh
```
