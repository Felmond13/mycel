# Kit pool chaud de microVMs (`bench/warm-pool`)

## Quoi

Le repli snapshot **multiplateforme** : quand le restore n'existe pas ou
n'est pas validé (WSL2-KVM/Cloud Hypervisor inert, libkrun sans API, macOS
HVF non mesuré), un pool de VMs **pré-bootées** sert des invités déjà
chauds. Ce ne sont que des processus et des taps : ça marche partout où la
microVM boote. Le kit mesure si la promesse tient, à charge identique
(benchhttp, noyau slim, ICMP-warm, même mètre que `bench/microvm`) :

1. **cold** — 5 échauffements + n=30 boots À FROID : T0 = lancement
   Cloud Hypervisor (création du tap incluse) → T1 = 1er octet d'un `200` ;
2. **pool** — POOL_SIZE=3 VMs pré-bootées (taps `mycP0..2`, subnets
   192.168.80-82.0/24, IP invité passée par cmdline `guest_ip=` — un seul
   initramfs pour tout le pool via `./mkinitramfs-pool.sh`), prêtes depuis
   > 5 s, vivacité vérifiée par battement de cœur console ; n=30 handoffs
   round-robin : T0 = curl déclenché → T1 = `200` reçu.

## Résultats mesurés (WSL2, 2026-11-04 — `results/verdict.txt`)

- **cold** : médiane 299,5 ms, p95 1431 ms (n=30, CH v54 direct + noyau
  slim, ICMP-warm inclus) ;
- **handoff** : médiane 28,1 ms, p95 61,2 ms (n=30, 3 VMs prêtes > 5 s) ;
- **accélération ×10,7** — le pool tient la promesse « invité déjà chaud »,
  même mètre que la série cold (spawn curl + TCP + 1er octet du `200`).

## Limites honnêtes (consignées dans le verdict)

- le pool détient POOL_SIZE invités **prêts** — 1 prêt par VM résidente ;
  le re-remplissage est un boot à froid **en tâche de fond** (coût = série
  cold) ;
- mémoire résidente : POOL_SIZE × 512 Mo pendant toute la vie du pool ;
- zéro dépendance au save/restore vCPU : c'est la propriété recherchée.

## Comment (1 commande)

```bash
sudo ./run.sh          # dans bench/warm-pool (Linux ou WSL2 avec /dev/kvm)
```

Auto-suffisant : réutilise/télécharge CH v54.0 comme `bench/snapshot-native`,
noyau `../microvm/assets/vmlinux-slim` (sinon `vmlinux`), génère l'initramfs
pool si absent. Pilotage : `RUNS=30 POOL_SIZE=3 sudo -E ./run.sh`.

## Prérequis

- Linux/WSL2 avec `/dev/kvm` ; root (tap) ;
- `curl`, `iproute2`, `ping`, `awk`, `grep`, `pgrep` ;
- assets frères `../microvm/assets/` (kernel + benchhttp).
