# Kit snapshot/restore sur KVM natif (`bench/snapshot-native`)

## Quoi

Une seule expérience auto-suffisante qui mesure, sur une machine Linux avec
un **KVM natif** (pas imbriqué), si le restore de microVM Cloud Hypervisor
v54.0 tient la promesse **sub-200 ms** :

1. **baseline** — n=30 boots à froid (T0 = lancement → T1 = 1er octet d'un
   `200` par curl, même protocole que `bench/microvm`) ;
2. **snapshot** — 1 boot, `pause`, `snapshot` vers un répertoire
   (`ch-remote snapshot file://<dir>`), durée de création consignée ;
3. **matrice restore** — `{copy, copyonwrite, ondemand} × {prefault=off, on}`,
   n=30 restores par case (`cloud-hypervisor --restore
   source_url=file://<dir>,memory_restore_mode=…,prefault=…,resume=true`),
   chaque run mesuré T0→T1 **et** vérifié vivant (battement de cœur console
   toutes les 200 ms de `benchhttp`) **et** nettoyé sans résidu (tap, VMM) ;
4. **verdict** — tableaux + GO/NO-GO : GO si une case donne
   ≥ 28/30 restores < 200 ms avec invité vivant.

## Pourquoi

Sous le KVM de WSL2, le restore charge la mémoire et relance le vCPU mais
l'invité reste **inert** (ni timer ni interruptions — mesure du 2026-11-03,
`docs/isolation-engines.md` §Snapshot) : impossible d'y trancher. La veille
(`docs/research/dedup-and-snapshot-survey.md` §B, R2) recommande CH v54
`memory_restore_mode=copyonwrite` + prefetch des pages chaudes (REAP,
cold start ÷3,7 publié). Ce kit apporte la preuve chiffrée sur KVM natif.

## Comment (1 commande)

```bash
sudo ./run.sh          # dans bench/snapshot-native, sur la machine KVM natif
```

Le script détecte root (re-exec via `sudo` sinon), vérifie `/dev/kvm`,
télécharge CH v54.0 (binaires statiques GitHub) s'il est absent ou d'une
autre version, réutilise `../microvm/assets/{vmlinux,rootfs.cpio}` s'ils
existent (sinon les construit via `../microvm/build_kernel.sh` +
`mkinitramfs.sh`), puis enchaîne les phases. Résultats dans `results/`
(`env.txt`, `*.times`, `matrix.txt`, `verdict.txt`).

Pilotage : `RUNS=30 RESTORE_RUNS=30 WARMUPS=5 RESTORE_WARMUPS=2
PHASES=baseline,snapshot,restore SNAPDIR=/chemin/sur/disque sudo -E ./run.sh`
(par ex. `RUNS=5 PHASES=baseline,snapshot` pour un test rapide).

## Prérequis

- Linux x86_64 avec **/dev/kvm** direct (bare-metal ou VM KVM pleine, pas
  WSL2 ni virtualisation imbriquée Hyper-V) ;
- root (création du tap, comme le daemon Docker du bench microvm) ;
- `curl`, `iproute2` (`ip`), `awk`, `grep`, `pgrep` ; `bash`, `date` GNU ;
- optionnel : `rustc` musl + `busybox-static` si les assets `../microvm`
  doivent être reconstruits.

## Ce que ça décide

- **GO** (une case ≥ 28/30 restores < 200 ms, invité vivant) : le chemin
  snapshot/restore CH v54 est validé → investir le pool-chaud / fork CoW
  façon SnowFlock/Catalyzer (recommandation R2 du survey) et intégrer
  `memory_restore_mode=copyonwrite` à `myc-backend`.
- **NO-GO** : le restore ne tient pas la promesse sur ce matériel → rester
  sur le fast-boot kernel (`bench/microvm`) et le store hardlink, remettre
  le S/R microVM aux calendes grecques.
- **Sortie « restore inert »** (exit 3) : l'hôte ne préserve pas l'état
  vCPU — c'est la signature WSL2 ; ré-exécuter sur un vrai KVM natif
  (protocole détaillé : `docs/benchmarks/snapshot-native-kit.md`).
