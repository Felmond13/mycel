# Firecracker jailer: hardening the snapshot accelerator (measured)

`MYCEL_FC_JAILER=1` launches the Firecracker snapshot path through the
upstream jailer of the same v1.17.0 release instead of a direct exec:
chroot + pivot_root into a per-run tree, private mount namespace, uid/gid
drop to a dedicated account, device nodes re-created inside the jail, and
Firecracker's own seccomp filter. This document is the evidence that the
hardening is REAL (process-level proofs) and that it costs nothing
measurable (hyperfine campaign, LOI: n≥30, median + p95).

How to enable (root required — chroot, mknod, tap ownership):

```bash
sudo MYCEL_FC_JAILER=1 myc run --engine kvm-microvm alpine:3.20 -- /bin/echo ok
```

Everything else is automatic: the jailer binary is provisioned from the
same release tarball as Firecracker on first use
(`~/.mycel/vm-assets/jailer`), the jail tree is built per run under
`<snapshots_root>/jails/` (hard links of the guest assets — instant even
for the 512 MiB `mem.img`; a cross-device snapshot root falls back to a
copy), and removed on exit. Jailed and direct runs use separate snapshot
caches (the vmstate embeds `/run/...` drive paths jailed vs host paths
direct — a `jailer=1` epoch in the fingerprint keeps them apart).
The default stays OFF (`MYCEL_FC_JAILER` unset or `0`).

## Isolation proofs (captured live, WSL2 dev machine)

A jailed run of `myc run --engine kvm-microvm alpine:3.20 -- /bin/sleep 8`
with `MYCEL_FC_JAILER=1`; the Firecracker process is probed through
`/proc` while the guest runs (pid 164717, run of 2026-11-05):

```
$ ps -o uid,gid,cmd -p 164717
UID   GID COMMAND
20000 20000 /firecracker --id 164696-0 --start-time-us … --log-path /run/fc.log --level Warning

$ grep -E 'Uid|Gid|Seccomp|CapEff|NoNewPrivs' /proc/164717/status
Uid:              20000 20000 20000 20000
Gid:              20000 20000 20000 20000
Seccomp:          2                      # filter active
Seccomp_filters:  1
CapEff:           0000000000000000       # zero capabilities
CapPrm:           0000000000000000
NoNewPrivs:       1

$ readlink /proc/164717/exe
/firecracker                          # the jailed binary, not a host path
$ ls /proc/164717/root/                # the chroot, seen from the host
dev  firecracker  firecracker.pid  run
$ ls /proc/164717/root/run/
fc.log  firecracker.socket  mem.img  rootfs.ext4  scratch.img  vmstate.bin
$ ls -R /proc/164717/root/dev/
/proc/164717/root/dev/:  kvm  net  urandom  userfaultfd
/proc/164717/root/dev/net:  tun
$ ls /proc/164717/root/home /proc/164717/root/etc
ls: … No such file or directory       # no host directory is visible
$ readlink /proc/164717/ns/mnt   → mnt:[4026532844]  (host: mnt:[4026532197] — private mount ns)
$ readlink /proc/164717/ns/net   → net:[4026531833]  (host: net:[4026531833] — same net ns, by design)
```

Reading it: the VMM runs as the dedicated uid 20000 (no login user can
ever match it — Linux assigns those from 1000), with zero effective or
permitted capabilities, `NoNewPrivs` set, one seccomp filter installed,
inside a chroot that contains ONLY the binary, `/dev` (kvm, net/tun,
urandom, userfaultfd) and `/run` (the socket, the log, and the hard-linked
guest drives) — `/home`, `/etc` and the host filesystem do not exist for
it. The network namespace is deliberately the host's (v1.17 removed
`--net-tap`; the host manages the tap/bridge — see below).

Functional proofs, same machine, all with the final binaries:

- jailed `echo ok` — exit 0, 8/8 runs across debug+release builds (the
  deep-path bench layout included);
- jailed cold boot + snapshot create, then jailed warm restores: internal
  spawn→resumed **20-25 ms** (trace metric);
- jailed `redis -p 6399:6379`: `+PONG` then SET/GET roundtrip through
  `127.0.0.1:6399` (portproxy + DNAT + bridge all functioning under the
  jail), zero residue after SIGTERM (no process, no tap, no DNAT rule,
  no jail tree);
- jailed egress: `apk add wget` + HTTPS 200 from a snapshot-restored
  jailed guest (the `myc-egress` NAT path);
- default-mode non-regression: `bench/microvm/e2e-vm.sh --with-ports`
  **27/27 PASS** with the jailer OFF.

## How networking survives the jailer (measured facts)

- The jailer v1.17 has no `--net-tap` (removed upstream; v1.17 only offers
  `--netns` to join an EXISTING namespace). mycel passes neither: the
  VMM stays in the host network namespace and opens the host-created tap
  `myc-<n>` (attached to `br-myc`) exactly like the direct path — the
  whole kvm.rs networking stack (leases, DNAT, portproxy, ARP seeding,
  ICMP warmup, egress NAT) is unchanged.
- Opening the tap needs `TUNSETIFF`, which a capability-less process may
  only do on a tap it owns. Jailed runs create the tap with
  `ip tuntap add … user 20000`; measured: uid 20000 attaches (OK), uid
  20001 gets `EPERM`.
- The tap is attached to the bridge BEFORE the VMM starts (host side,
  as root) — no namespace move happens, so the bridge membership holds.

## WSL2 specifics discovered along the way (documented, worked around)

- **`/tmp` is tmpfs mounted `nodev`**: every device node the jailer mknods
  inside a jail that lives under `/tmp` fails to open with `EACCES` — for
  the VMM AND for root (first symptom: `Couldn't open /dev/net/tun:
  Permission denied`). The jail therefore lives under the snapshots root
  (`~/.mycel/snapshots/jails/…` — same device as the cache, so the guest
  assets hard-link instantly). This is a mount flag, not a jailer bug;
  native Linux distros with the same tmpfs hardening behave identically.
- **Unix socket paths are capped at 107 bytes (`sun_path`)**: a deep
  snapshot root (hermetic bench layout inside the worktree) pushes
  `<jail>/root/run/firecracker.socket` past the cap, and every connect
  fails with `path must be shorter than SUN_LEN` — the engine then waits
  out its API timeout while the VMM is perfectly healthy (measured: the
  socket exists and serves; only the client cannot address it). The jail
  id is kept short and the base falls back to `<home>/.mycel/jails` when
  the preferred base overflows the cap.
- **Kernel-delivered SIGHUPs (`SIGHUP {si_code=SI_KERNEL}`, strace)
  killed jailed VMMs seconds after spawn** (exit 156, `Shutting down VM
  after intercepting signal 1`): WSL session/ConPTY churn hangs up the
  foreground process group; myc swallows the signal via its ctrlc handler
  but Firecracker's own SIGHUP handler shuts the VM down. The VMM now
  spawns in its own process group (`process_group(0)`) — the same trick
  the jailer itself plays (setsid) for `--new-pid-ns`. Without it the
  failure was intermittent (30/30 under hyperfine, sporadic standalone).
- **A failed attempt used to leak the VMM**: if the API never answered,
  the dropped `Child` handle killed nothing — a dozen idle jailed
  Firecrackers accumulated during one debugging session. The failure
  paths now kill the child explicitly, and a cold retry kills the
  previous attempt's VMM before respawning.
- **cgroups v2 without delegation**: the jailer only touches cgroups when
  `--cgroup` limits are passed (`cgroup_conf` is `Option`; setup is
  skipped entirely when absent — read from the v1.17 source). mycel
  passes none, so no cgroup path is exercised and none can fail; when
  limits become policy later, the knob exists (`--cgroup cpu.shares=…`
  etc., plus `--cgroup-version 2`).
- **Seccomp works under WSL2-KVM** (`Seccomp: 2`, one filter — the same
  filter Firecracker installs on the direct path; it is Firecracker's
  own, applied jailed or not).

## Benchmark: what the jailer costs

Protocol (`sudo bash bench/fc-jailer/run.sh`, LOI: hyperfine, n=30 per
scenario, median + p95 computed from the raw times; every measured run
VERIFIES output `ok` + exit code 0, so a silently broken run fails the
bench, never a number):

| scenario | what it measures |
|---|---|
| (a) warm jailed | `MYCEL_FC_JAILER=1`, snapshot restored (hard links in, jailer exec, vmstate load, resume) |
| (b) warm direct | default mode, snapshot restored (the shipped behavior) |
| (c) cold jailed | cache wiped before every run — jailer overhead drowned in a full boot + snapshot create |
| (d) CH fallback | `MYCEL_SNAPSHOT_CACHE=0` — Cloud Hypervisor voie B (context) |

Environment (archived verbatim in `bench/fc-jailer/results/env.txt`):
WSL2 kernel 6.18.54.1-microsoft-standard-WSL2, 11th Gen Intel Core
i5-1145G7 @ 2.60 GHz, myc 0.3.1 (debug build), Firecracker v1.17.0 +
Jailer v1.17.0, image alpine:3.20, 2026-11-05.

### Results (n=30 each, end-to-end `myc run … -- /bin/echo ok`)

| scenario | median | p95 | mean ± σ |
|---|---|---|---|
| (a) warm restore **jailed** (`MYCEL_FC_JAILER=1`) | **224.2 ms** | **268.8 ms** | 230.7 ± 24.7 ms |
| (b) warm restore direct (shipped default) | 243.9 ms | 344.5 ms | 257.3 ± 43.8 ms |
| (c) cold boot jailed (boot + snapshot create) | 1718.8 ms | 2349.2 ms | 1767 ± 257 ms |
| (d) Cloud Hypervisor fallback (cache off) | 435.6 ms | 510.3 ms | 431.3 ± 37.3 ms |

Internal trace metric (spawn → resumed, `MYCEL_VM_TRACE=1`): jailed
restores land at **20-25 ms** — the end-to-end numbers are dominated by
engine setup (materialize check, lease, jail hard links) and process
 teardown, identical on both paths.

### Verdict

**The jailer is free.** The jailed median is 19.7 ms LOWER than direct
(x0.92) — inside run-to-run σ (the two runs interleave on a live WSL2
machine, not a benchmark-isolated box) — and the jailed p95 is 75.7 ms
BETTER (268.8 vs 344.5 ms: the jailed series is tighter). No scenario
showed a jailer-induced regression; cold boots absorb the jail setup
(chroot tree + 4 MB binary copy + mknod) invisibly. The snapshot restore
stays ~2× faster than the Cloud Hypervisor fallback jailed or not.

Recommendation for the default: keep `MYCEL_FC_JAILER=0` for one more
cycle of exposure (this ticket's policy), then flip it — the hardening
(chroot, uid 20000, zero caps, NoNewPrivs, private mount ns, seccomp) is
strictly additive at zero measured latency cost.
