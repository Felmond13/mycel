# Kit Firecracker snapshot/restore sous WSL2 (`bench/snapshot-wsl2`)

## Quoi

Le pendant Windows/WSL2 de `bench/snapshot-native` : le restore Cloud
Hypervisor est **inert** sous le KVM de WSL2 (l'invité ne reçoit plus ni
timer ni interruptions — `docs/isolation-engines.md` §Snapshot), mais
Firecracker emprunte un chemin save/restore vCPU **distinct** : il faut le
mesurer au lieu de le supposer. Ce kit tranche, chiffres à l'appui :

1. **boot** — n=5 boots à froid Firecracker (T0 = lancement → T1 = 1er
   octet d'un `200` par curl, ICMP-warm inclus comme `bench/microvm`) ;
2. **snapshot** — 1 boot, `PATCH /vm Paused`, `PUT /snapshot/create`
   (Full), durée de création consignée ;
3. **restore** — n=10 restores via `PUT /snapshot/load` (mem_backend File)
   + `PATCH /vm Resumed` (la CLI v1.17 n'a plus de `--snapshot-path`),
   chaque run mesuré T0→T1 **et** vérifié vivant (battement de cœur console
   toutes les 200 ms de `benchhttp` sur ttyS0) **et** nettoyé sans résidu ;
4. **verdict** — VIVANT (le chemin snapshot Windows existe, chiffres) ou
   INERT (documenté, repli = pool chaud `bench/warm-pool`).

## Résultats mesurés (WSL2, 2026-11-04 — `results/verdict.txt`)

- boot à froid : médiane 389 ms (n=5) ;
- création snapshot : 614 ms (512 Mo) ;
- **restore : 10/10 vivants, médiane 91,2 ms, p95 160,9 ms** — sous la
  barre des 200 ms. Firecracker restore VIVANT là où CH v54 est inert :
  le chemin snapshot/restore **existe sous Windows/WSL2**.

## Comment (1 commande)

```bash
sudo ./run.sh          # dans bench/snapshot-wsl2, sur WSL2 avec /dev/kvm
```

Auto-suffisant : télécharge Firecracker v1.17.0 (release GitHub) si absent
(`FC_VERSION` pour changer), récupère le **vmlinux ELF non compressé** du
noyau slim (Firecracker ne charge pas les bzImage) depuis l'arbre de build
`~/build/mycel-kernel/build-slim/` ou par extraction du bzImage, et génère
l'initramfs dédié via `./mkinitramfs-fc.sh` (IP 192.168.78.2, sortie PID 1
sur ttyS0 — la console FC est un UART 8250, pas hvc0). Topologie dédiée :
tap `mycf0` 192.168.78.1/24.

Pilotage : `PHASES=boot,snapshot,restore BOOT_RUNS=5 RESTORE_RUNS=10
sudo -E ./run.sh` (ex. `PHASES=boot BOOT_RUNS=2` pour un smoke test).

## Prérequis

- WSL2 (ou Linux) avec `/dev/kvm` ; root (tap) ;
- `curl`, `iproute2`, `ping`, `awk`, `grep`, `pgrep`, `python3` ;
- assets frères `../microvm/assets/benchhttp` (via `mkinitramfs.sh`).
