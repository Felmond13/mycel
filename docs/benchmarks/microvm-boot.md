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

## Campaign v3 — outlier fix + slim kernel

Two changes, measured separately, same protocol as v2 (root, ABBA,
n = 30/series, hyperfine, same machine, 2026-11-04, tree at `4daf49d`
+ these changes):

1. **ICMP warm-up (kills the ~9 s outlier).** The guest *kernel* answers
   ICMP echo as soon as eth0 has its IP — no userspace needed. The bench
   (`run_until_ready.sh`, microvm branch) now streams one ICMP echo every
   20 ms (iputils 20250605: `-W` is in whole seconds, so one-shot failed
   pings would block a full second — hence a continuous stream + `grep
   -m1`, first reply in ~20 ms) and only sends the first TCP SYN after
   the first ICMP reply. A SYN sent before eth0 is up is silently dropped
   (permanent ARP entry) and the SYN backoff (~1+2+4 s) was the v2
   outlier. T0 is unchanged (launch), the warm-up runs *inside* the
   measured T0→T1 window. `KvmMicroVm::wait_ready` (kvm.rs) got the same
   ICMP-first logic behind the same public API; `examples/boot` prints
   the same "guest ready" line (the bench timer is the curl loop, not
   wait_ready).
2. **Slim kernel** (`build_kernel_slim.sh`): same Linux 6.12.115, built
   from `allnoconfig` + an explicit fragment (append + `olddefconfig`,
   no `merge_config.sh`) — 64BIT/SMP, PRINTK, 8250+console, PCI(+MSI),
   all virtio devices built-in, initramfs (uncompressed cpio), INET/UNIX,
   **IPV6 off**, PROC/SYSFS/DEVTMPFS/TMPFS, ELF+script binfmt, futex/
   epoll/signalfd/timerfd/eventfd, ACPI + IO-APIC (they alone describe
   interrupts on Cloud Hypervisor x86_64), KVM_GUEST/PARAVIRT (kvmclock),
   LZ4. Two traps worth remembering: `allnoconfig` omits the lines of
   `default y` symbols (olddefconfig then re-enables them — IPV6, CPU
   freq, thermal… need explicit `# … is not set`), and `VIRTIO_PCI`/
   `VIRTIO_BLK` hide behind the `VIRTIO_MENU` / `BLK_DEV` menus.
   Output: `assets/vmlinux-slim` (**4 051 968 bytes ≈ 3.9 MiB, −70.6 %**,
   sha256 `c1efb844…`); `assets/vmlinux` (13 804 544 bytes ≈ 13.2 MiB)
   is untouched and remains the default engine kernel. The slim variant
   is selected at campaign time via `MYCEL_KERNEL=…/vmlinux-slim`
   (recorded in `env.txt` of each campaign).

### v3-stock — warm-up fix + stock 13.2 MiB kernel

| Series | median | p95 | max | CV | runs > 800 ms |
|---|---|---|---|---|---|
| A — docker | 331.7 ms | 386.2 ms | 445.5 ms | 7.5 % | 0 |
| C — microvm | 420.9 ms | 424.0 ms | **424.3 ms** | **0.4 %** | 0 |
| C_b — microvm | 420.6 ms | 520.9 ms | 521.8 ms | 5.9 % | 0 |
| A_b — docker | 322.8 ms | 358.3 ms | 364.8 ms | 4.4 % | 0 |

The 9 s outlier is **gone** (v2: 2/60 at ~9 045 ms; v3: 0/60, worst run
521.8 ms). Series C is essentially deterministic: 30/30 runs in a 6 ms
window, CV 0.4 %. The warm-up itself is not free — the median settles
~16 ms above v2's 404-410 ms — a flat, tail-free price.

### v3-slim — warm-up fix + slim 3.9 MiB kernel

| Series | median | p95 | max | CV | runs > 800 ms |
|---|---|---|---|---|---|
| A — docker | 345.3 ms | 372.1 ms | 415.5 ms | 5.8 % | 0 |
| C — microvm | **118.3 ms** | 220.5 ms | 222.8 ms | 30.2 % | 0 |
| C_b — microvm | **118.0 ms** | 318.5 ms | 320.3 ms | 40.2 % | 0 |
| A_b — docker | 321.2 ms | 337.3 ms | 362.2 ms | 3.0 % | 0 |

Same kernel version, minimal config: median boot **420.7 → 118.1 ms
(−72 %, ×3.6)**, and the microVM now beats same-day Docker by ×2.7 on
the median (118 vs 321-345 ms) — with a full dedicated kernel. Zero-queue
confirmed across the whole campaign: **0 of 120 microvm runs above
800 ms** in v3 (60 stock + 60 slim), worst 521.8 ms.

Honest asterisks:

- **The slim series is bimodal.** 23/30 runs per series land in a
  near-deterministic 117-118 ms floor; the rest form a thin tail at
  200-320 ms (roughly +100/+200 ms steps — nested-KVM scheduling
  hiccups that were invisible at 420 ms become visible at this
  timescale). Hence the high CV (30-40 %) despite the better median;
  the worst slim run (320.3 ms) still beats every same-day Docker
  median.
- **Docker's baseline wanders between days** (v2: 455-480 ms, v3:
  321-345 ms — same daemon, warm). Only same-day ABBA halves are
  comparable; both are published.
- Boot-median conclusions hold on this WSL2/nested-KVM host; the
  absolute floor should drop further on native KVM.

Raw files: `results-v3/stock/` and `results-v3/slim/` (next to this
report). Reproduce:

```bash
cd bench/microvm
KERNEL_VERSION=linux-6.12.115.tar.xz ./build_kernel_slim.sh   # assets/vmlinux-slim
sudo env RESULTS=results-v3-stock bash run.sh                 # warm-up + stock kernel
sudo env RESULTS=results-v3-slim \
     MYCEL_KERNEL=$PWD/assets/vmlinux-slim bash run.sh        # warm-up + slim kernel
```
