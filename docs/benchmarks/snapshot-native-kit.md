# Kit d'expérience — snapshot/restore microVM sur KVM natif

> Kit exécutable : [`bench/snapshot-native/run.sh`](../../bench/snapshot-native/run.sh)
> (protocole d'usage dans son README). Ce document fixe le protocole, les
> résultats attendus d'après la littérature, et le verdict WSL2 déjà établi.
> Établi le 2026-11-03 ; toute exécution archive ses chiffres dans
> `bench/snapshot-native/results/` sans retoucher ce document.

## 1. Question tranchée

La veille ([dedup-and-snapshot-survey.md §B, R2](../research/dedup-and-snapshot-survey.md))
recommande : **Cloud Hypervisor v54, `memory_restore_mode=copyonwrite` +
prefetch des pages chaudes** (approche REAP [Ref20] — record des défauts de
page d'un restore précédent puis préchargement : **cold start ÷3,7** publié,
empreinte mémoire −61 à −96 %). Cette recommandation repose sur des mesures
de laboratoire et la doc CH [Ref44] ; elle n'a **jamais été validée sur nos
machines**. La question : *un restore microVM peut-il servir de démarrage
quasi gratuit (< 200 ms, p95 < 200 ms, invité vivant) pour Mycel ?* — ce qui
décide de l'investissement **pool-chaud / fork CoW** (SnowFlock [Ref23],
sfork Catalyzer [Ref19]) versus fast-boot kernel simple.

### Verdict WSL2 déjà établi (2026-11-03, mesure Mycel)

Sur le KVM **imbriqué de WSL2** : snapshot ~0,3 s / 512 MiB, restore charge
la mémoire et relance le vCPU, **mais l'invité reste inert** — ni timer, ni
interruptions — pour les trois modes mémoire (`copy`, `copyonwrite`,
`ondemand`). Le KVM de WSL2 ne préserve pas l'état vCPU/MSR au
save/restore ([isolation-engines.md §Snapshot](../isolation-engines.md),
constat Capsule M0). **Conséquence : toute la matrice restore de ce kit
échoue proprement sous WSL2** avec le message « RESTORE INERT » (exit 3) —
c'est l'auto-vérification du détecteur d'inertie, pas un bug. Le kit doit
donc être exécuté sur un **KVM natif** (bare-metal, cloud VM KVM direct) :
c'est tout l'objet de l'expérience.

## 2. Protocole

Mêmes discipline et charge que le bench microVM ([isolation-engines.md
§bench](../isolation-engines.md)) : noyau Linux 6.12 piné (virtio intégré),
initramfs busybox + `benchhttp` (binaire statique musl, `200 ok\n` sur
`192.168.77.2:8080`, **battement de cœur console `benchhttp: heartbeat`
toutes les 200 ms**), tap hôte `myc0` (192.168.77.1/24) joint directement —
aucun proxy userland. Flags VMM identiques à
`crates/myc-backend/src/kvm.rs` (`--cpus boot=1 --memory size=512M --net
tap=myc0,mac=52:54:00:77:00:02,ip=192.168.77.1,mask=255.255.255.0
--cmdline "console=hvc0 quiet"`), entrée ARP permanente pré-seedée dès que
le tap existe (sinon le p95 part à ~1 s — cf. microvm-tail-analysis).

Métrique : T0 = lancement de la commande → T1 = premier octet d'un `200`
reçu par le client (boucle curl 5 ms, même boucle que
`bench/microvm/run_until_ready.sh`) ; médiane + p95 + CV via
`bench/microvm/stats.sh` sur les `.times` bruts en nanosecondes.

1. **Baseline** : `WARMUPS=5` échauffements non mesurés puis `RUNS=30` boots
   à froid. Chaque run vérifie aussi le battement de cœur (auto-test du
   détecteur de vivacité) et laisse zéro résidu (tap, processus VMM) — tout
   résidu fait échouer le run.
2. **Snapshot** : 1 boot, 1,5 s de stabilisation, `ch-remote pause`
   (obligatoire en v54), `ch-remote snapshot file://<dir>` — destination =
   **répertoire** ; durée de création consignée (`results/snapshot-info.txt`,
   avec la liste des fichiers produits : `config.json`, `state.json`,
   mémoire, `memory-ranges`).
3. **Matrice restore** : pour chaque `memory_restore_mode ∈ {copy,
   copyonwrite, ondemand}` × `prefault ∈ {off, on}` (le prefault, sous
   forme de `--restore
   source_url=file://<dir>,memory_restore_mode=<m>,prefault=<p>,resume=true`,
   ne concerne en v54 que le chemin `copy` ; les cases sont mesurées quand
   même pour documenter le comportement réel) : `RESTORE_WARMUPS=2`
   échauffements puis `RESTORE_RUNS=30` restores. Chaque run :
   - tap résiduel supprimé avant lancement ;
   - T0→T1 par la boucle curl (budget 15 s) ;
   - **vivacité** : ≥ 2 nouveaux `heartbeat` dans la console pendant une
     fenêtre de 1,2 s — un invité restauré *figé* (chargé mais sans timer)
     n'écrit plus rien : c'est LE test qui a tranché sous WSL2. Les deux
     fichiers console possibles sont surveillés (celui du run et celui,
     mort, de la VM snapshotée) car la reprise de config console au restore
     peut réouvrir l'un ou l'autre ;
   - classification : `ok` (200 + vivant), `inert` (VMM vivant, pas de 200,
     aucun battement — signature WSL2), `netdead` (invité vivant mais
     injoignable, ou 200 puis figé), `died` (VMM mort : restore refusé,
     ex. `ondemand` sans userfaultfd), `leak` (résidu tap/VMM après
     nettoyage — échec immédiat) ;
   - arrêt par `ch-remote shutdown-vmm` (une VM en pause ne peut pas faire
     un shutdown gracieux), puis kill si besoin, puis vérification
     anti-fuite (tap absent, aucun processus VMM).
   - **interruption propre** : 3 restores `inert` consécutifs (paramètre
     `INERT_ABORT`) abortent la matrice avec le message « RESTORE INERT —
     l'hyperviseur hôte ne préserve pas l'état vCPU » et l'exit 3 : échec
     propre, pas de plantage ni de 180 runs perdus.
   - `ondemand` exige userfaultfd (`/dev/userfaultfd` ou
     `vm.unprivileged_userfaultfd=1` [Ref44]) : sans lui la case est
     déclarée `SKIP` avec la raison.
4. **Verdict** (`results/verdict.txt`) : tableau baseline + tableau matrice
   (ok / inert / autres, médiane, p95, n<200 ms, accélération vs baseline)
   puis GO/NO-GO.

### Règle de décision

**GO** si au moins une variante atteint **≥ 28/30 restores < 200 ms avec
invité vivant**. Le seuil de 200 ms est le budget « démarrage quasi
gratuit » de Mycel (le boot optimisé actuel fait ~400 ms médian sous WSL2,
voir ci-dessous ; un restore ne se justifie que s'il bat nettement le
boot). GO → intégrer le pool-chaud/fork CoW (R2 du survey) et le mode
`copyonwrite` dans `myc-backend`. NO-GO → rester sur le fast-boot kernel et
le store hardlink ; le S/R microVM retourne en roadmap longue.

### Conventions et limites du protocole

- Le snapshot vit par défaut dans `/tmp` (tmpfs sur la plupart des
  machines) : le restore mesure alors le **meilleur cas** I/O (RAM).
  `SNAPDIR=/chemin/sur/disque sudo -E ./run.sh` pour mesurer le cas disque
  (froid = `echo 3 > /proc/sys/vm/drop_caches` entre les runs, à la main —
  non fait par défaut).
- Comme Firecracker [Ref35], un même snapshot repris N fois n'est pas
  « production-safe » (unicité PRNG/TCP) : ce bench mesure la latence, pas
  la sécurité — la production Mycel exige VMGenID + ré-amorçage d'entropie
  (R2.3 du survey).
- Charge statique identique au bench microvm : la comparaison
  baseline↔restore est à poids égal.

## 3. Résultats attendus (littérature)

| Signe | Attendu | Source |
|---|---|---|
| baseline boot à froid | ~300-500 ms médian (410 ms médian mesuré sur le poste WSL2, bench microvm `C.times`, 2026-11-03) | mesure Mycel [Ref46] ; Firecracker < 125 ms avec kernel+init élagués [Ref16] |
| création du snapshot | ~300 ms pour 512 MiB (constat WSL2) ; ~1 s si snapshot sur disque | mesure Mycel [Ref46] |
| restore `copy` | dominant par la lecture 512 MiB : des centaines de ms à qq s selon stockage ; prefault=on déplace les défauts hors du chemin mesuré | doc CH [Ref44] ; « prefault » du survey §B.3 |
| restore `copyonwrite` | le plus rapide au démarrage : zéro copie up-front (mmap privé), défauts servis par le page cache ; c'est la variante candidate GO | doc CH [Ref44] ; MAP_PRIVATE Firecracker [Ref35] |
| restore `ondemand` | rapide au T1 mais pages servies au fil des défauts (uffd) — latence décalée après la mesure ; exige userfaultfd | doc CH [Ref44] ; CRIU lazy-pages [Ref45] |
| ordre de grandeur restore (VMM optimisé) | « aussi peu que 4 ms, ~10 ms pour un Linux complet » chez Firecracker/Brooker ; Lambda SnapStart jusqu'à 10× le cold start Java | [Ref17][Ref37] |
| prefetch pages chaudes | cold start ÷3,7, mémoire −61 à −96 % | REAP [Ref20] |
| sous WSL2 (imbriqué) | matrice entièrement `inert` → kit aborte proprement (exit 3) | mesure Mycel [Ref46] |

Résultat ATTENDU sur KVM natif sain : `copyonwrite-prefault-off` ou `-on`
sous 200 ms médian et p95 (la mémoire reste dans le page cache), GO. Si le
matériel sert le snapshot depuis un disque froid, `copy`+prefault=on et
`copyonwrite` dégradent vers plusieurs centaines de ms — le verdict dira
 lequel des deux mondes nous occupe.

## 4. Auto-vérification exécutée (WSL2, horloge hôte 2026-11-04T01:55Z)

Le kit a été exécuté tel quel sur le poste WSL2 (i5-1145G7, 8 vCPU,
11 Gio, KVM imbriqué) avec `RUNS=10 WARMUPS=3` — phases baseline et
snapshot devaient marcher, la matrice restore devait échouer *proprement*
en INERT. C'est exactement ce qui s'est passé (archive :
`bench/snapshot-native/results/`) :

| Phase | Résultat |
|---|---|
| baseline | n=10, **médiane 423,4 ms**, p95 569,7 ms, CV 11,2 % — cohérent avec le bench microvm historique (médiane ~410 ms, `C.times`) ; battements vérifiés à chaque run |
| snapshot | **créé en 233 ms** pour 512 MiB (conforme au ~0,3 s du constat initial) ; fichiers `config.json` + `state.json` + `memory-ranges` (512 MiB) |
| restore | `copy-prefault-off` : 3 restores consécutifs **inert** (2 échauffements + 1 mesure) → matrice interrompue proprement, message « RESTORE INERT — l'hyperviseur hôte ne préserve pas l'état vCPU », verdict NO-GO, **exit 3** ; signature dans `failures.log` : VMM vivant (avertissement vcpu0 dans ch.log), console **muette**, aucun 200 |

Le détecteur d'inertie (battement de cœur console) est donc validé dans les
deux sens : invité vivant détecté à chaque boot baseline, invité figé
détecté à chaque restore. Note de mesure : sur ce poste partagé, un
`cargo build` concurrent a fait grimper la baseline à 1,6-5,8 s médian
pendant quelques essais — les chiffres archivés ci-dessus sont pris à
charge retombée (l'environnement complet, `uptime` inclus, est dans
`results/env.txt`). Sur KVM natif, exécuter à machine calme.

## 5. Références

[Ref16] Firecracker, NSDI'20 · [Ref17] Brooker, SnapStart · [Ref19]
Catalyzer, ASPLOS'20 · [Ref20] REAP, ASPLOS'21 · [Ref23] SnowFlock,
EuroSys'09 · [Ref35] Firecracker snapshot support · [Ref37] Lambda SnapStart ·
[Ref44] Cloud Hypervisor, docs snapshot_restore.md · [Ref45] CRIU lazy
migration · [Ref46] Mycel docs/isolation-engines.md + Capsule M0 — toutes
résolues dans la
[bibliographie du survey](../research/dedup-and-snapshot-survey.md#bibliographie).
