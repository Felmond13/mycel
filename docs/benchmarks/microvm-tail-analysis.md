# Analyse de la queue de latence — boot microVM (séries C/C_b)

Analyse de la dispersion du boot microVM mesurée dans `microvm-boot.md`
(CV ~35 %, p95 ~1060 ms contre 5,9 % / 447 ms pour la série Docker propre),
avec investigation instrumentée du 2026-11-04 (WSL2 rebootée, même machine,
mêmes binaires). Aucune modification du code : toutes les mesures
additionnelles ont été faites en pilotant `cloud-hypervisor` directement
( arguments identiques à `kvm.rs` ) ou le binaire `examples/boot` tel quel.

## 1. Chiffres — séries publiées (`bench/microvm/results/*.times`, n = 30)

| Série | min | p50 | p90 | p95 | p99 | max | moyenne | CV |
|---|---|---|---|---|---|---|---|---|
| A — docker (h1) | 384,5 | 469,2 | 1299,1 | 1354,1 | 2117,2 | 2117,2 | 674,3 | 64,2 % |
| A_b — docker (h2) | 356,7 | 375,5 | 399,3 | 447,1 | 455,6 | 455,6 | 383,7 | 6,0 % |
| C — microvm (h1) | 393,8 | 416,0 | 571,8 | 1056,3 | 1076,5 | 1076,5 | 475,2 | 35,4 % |
| C_b — microvm (h2) | 392,4 | 428,2 | 754,5 | 1074,4 | 1075,7 | 1075,7 | 507,7 | 37,0 % |

(p50 = médiane classique ; p90/p95/p99 = rang inférieur. Le tableau de
`microvm-boot.md` semble avoir croisé les libellés C et C_b par rapport aux
fichiers — sans effet : les deux séries sont statistiquement équivalentes.)

Histogramme (buckets 100 ms) — microVM : corps dense à 400–500 ms
(23 + 19 runs), 1–4 runs par bucket intermédiaire (500–900 ms), puis
**2 + 2 runs dans 1000–1100 ms**. Docker A_b : 27/30 dans 300–400 ms.

Corps hors queue (runs < 600 ms uniquement) :

| Série | n | p50 | p90 | max | CV |
|---|---|---|---|---|---|
| C | 27 (90 %) | 413,3 | 465,8 | 571,8 | **8,5 %** |
| C_b | 24 (80 %) | 416,5 | 475,9 | 511,1 | **7,2 %** |
| A_b (référence) | 30 (100 %) | 375,5 | 399,3 | 455,6 | 6,0 % |

**Hors queue, le boot microVM est presque aussi stable que Docker** (CV 7–8,5 %
contre 6 %). Toute la dispersion vient de 9 runs sur 60 qui prennent +200 à
+680 ms au-dessus du plancher.

## 2. Dérive, périodicité, artefact Docker

- **C / C_b : aucune dérive.** Spearman ordre↔durée : −0,23 / +0,02 ; pentes
  −4,1 / −0,3 ms/run ; médianes des moitiés à ±1,7 %. La queue est
  **événementielle** (runs isolés), pas une dégradation continue.
- **Pas de motif périodique.** Index des tails > 600 ms : C → 6, 11, 12
  (intervalles 5, 1) ; C_b → 1, 9, 10, 15, 25, 27 (intervalles 8, 1, 5, 10, 2).
  Aucune période dominante → pas de housekeeping/GC périodique.
- **Artefact Docker quantifié (série A)** : bascule nette au run 23 — les 22
  premiers runs sont propres (max 574,4 ; médiane 425,7 ; CV 13,4 %), les 8
  derniers s'effondrent (min 791,9 ; médiane 1273,1 ; max 2117,2 ; pente
  +36,6 ms/run, Spearman +0,73). Une perturbation externe transitoire
  (~15 s, WSL/daemon fraîchement redémarrés) — disparue pour A_b. Même hors
  dérive, A reste 2× plus bruitée qu'A_b (13,4 % vs 6,0 %) et +13 % de
  médiane : la comparaison doit se faire contre A_b, comme le fait le doc.
- **Plateau révélateur** : les gros tails convergent vers 1056–1077 ms
  (4 valeurs à ±1 %), retrouvés à 1047–1161 ms (×11) et 1045–1096 ms (×7)
  dans les campagnes du 04/11. Un « mur » si reproductible (±2 % sur 4
  protocoles) est la signature d'un **timer déterministe**, pas d'un bruit de
  contention.

## 3. Investigation du 2026-11-04 (protocoles et preuves)

Toutes les données brutes et scripts : `/home/nboukadoum-ext/build/`
(`an_stats.sh`, `an_boot30.sh`, `an_paired.sh`, `an_console.sh`, `an_arp.sh`,
`an_arpfix*.sh`, sorties dans `build/boot30/`).

| Hypothèse | Test | Verdict |
|---|---|---|
| Steal time Hyper-V/KVM imbriqué | delta `/proc/stat` (champ steal) par run, 30 boots | **Réfuté comme mesure** : steal = 0 en permanence (WSL2 ne le reporte pas) ; et le plateau déterministe exclut une contention aléatoire |
| THP / compaction mémoire | deltas `/proc/vmstat` (compact_stall/fail, thp_fault_alloc, pgmigrate) | **Réfuté** : tous à 0 (THP en `madvise`, CH n'en demande pas) |
| Biais de la sonde `curl --max-time 0.25` | 20 runs, deux sondes en parallèle sur le même boot (250 ms vs 50 ms) | **Innocentée** : écarts ≤ 13 ms ; les deux détectent le plateau ensemble |
| Migration vCPU / scheduling | `taskset -c 2,3` autour de boot+VMM (affinité héritée), 15 runs | **Sans effet sur la queue** : 3 tails/15, plateau à 1063–1068 ms inchangé |
| Boot kernel réellement lent | CH lancé directement, `printk.time=1`, console horodatée côté hôte | Le kernel est prêt tôt et de façon stable : `Run /init` à 0,21 s (temps kernel), `benchhttp: ready` ~450–670 ms ; **l'invité écoute avant la détection**. Console bavarde = coûteuse (tous les runs ≥ 1043 ms, jusqu'à 2300 : chaque printk = VM exit virtio-console sous KVM imbriqué) |
| **Premier contact réseau (ARP)** | journal `ip neigh` échantillonné toutes les 12 ms pendant 10 runs | **CAUSE PROUVÉE** : voir ci-dessous |

### La cause racine : résolution ARP du premier paquet + timer `neigh` ~1 s

- L'entrée `192.168.77.2` passe `INCOMPLETE` dès 11–130 ms (le tap existe
  avant que la pile invité monte) : la première sollicitation ARP part quand
  l'invité n'a pas encore configuré `eth0`, elle échoue.
- Runs rapides : `REACHABLE` à 376–378 ms → T1 = 387–388 ms.
- Runs lents : l'entrée reste `INCOMPLETE` jusqu'à la **retransmission à
  ~+1 s** (`retrans_time_ms = 1000`) : `REACHABLE` à 1035 / 1182 ms →
  T1 = 1037 / 1209 ms. **T1 coïncide au millième de seconde près avec le
  passage REACHABLE.**
- Le plateau bimodal 1050–1090 ms = « 1re sollicitation ratée + 1 s de timer
  neigh ». Les tails intermédiaires (620–830 ms) sont les cas limites où la
  re-résolution partielle arrive plus tôt.

### Fix validé expérimentalement

Le MAC invité est déjà codé en dur dans `kvm.rs`
(`52:54:00:77:00:02`) : une entrée ARP **permanente** posée dès la création
du tap élimine toute résolution :

```
ip neigh replace 192.168.77.2 lladdr 52:54:00:77:00:02 dev myc0 nud permanent
```

Résultat (12 runs, 3 warmups, même orchestrateur) :
**326–419 ms, p50 343 ms, max 419 ms, 0 tail** — soit meilleur que la série
Docker propre (p50 375,5 / p95 447,1). Résidu observé : les tout premiers
runs après une période d'inactivité restent plus lents (1363, 1022 ms sur
les 2 premiers d'une série froide) — léger transitoire cache/états CPU,
gommés par les warmups, à surveiller avec n plus grand.

### Bug de mesure trouvé au passage

`KvmMicroVm::wait_ready()` retourne `deadline.elapsed()` où `deadline` est
dans le futur (`start + 20 s`) : la soustraction sature et `boot.rs` affiche
`guest ready in 0ns` sur tous les runs. La boucle fonctionne, mais la mesure
interne du temps de boot est inutilisable en l'état (à corriger : elapsed
depuis un `Instant` pris avant la boucle).

## 4. Diagnostic hiérarchisé

1. **Queue microVM (p95 ~1060 ms) = résolution ARP du premier contact**
   (preuve causale §3). Le boot réel est stable ~330–420 ms.
2. **Artefact Docker série A** = perturbation externe transitoire (runs
   23–30) + settling du daemon (bruit ×2, +13 % de médiane vs A_b).
   La comparaison honnête est C/C_b vs A_b — c'est déjà ce que publie le doc.
3. **Coûts structurels sous KVM imbriqué WSL2** (plancher, pas la queue) :
   décompression d'un bzImage defconfig de 14 Mo + initcalls nombreux
   (e1000/e1000e/sky2/usb/ALSA/DRM/cfg80211 visibles en console) + VM exits
   virtio-console. La verbosité console multiplie la latence par ~3–5.
4. **Réfutés** : steal (non reporté, et plateau déterministe), THP/compaction
   (deltas nuls), biais de sonde curl (apparié ≤ 13 ms), migration vCPU
   (taskset sans effet sur la queue).

## 5. Pistes d'optimisation classées

| # | Piste | Impact p95 attendu | Coût | Risque | Preuve/littérature |
|---|---|---|---|---|---|
| 1 | **ARP permanent** côté hôte dès la création du tap (MAC déjà fixe) ; variante : gratuitous ARP côté invité (`busybox arping -U`) | **p95 1060 → ~420-450 ms (−58 %)**, CV 35 → <10 % | Trivial (2 lignes dans `launch()`) | Quasi nul | Validé ici : 12/12 runs ≤ 419 ms |
| 2 | **Corriger `wait_ready`** (0 ns) et publier deux métriques : boot kernel (timestamps console) vs premier contact réseau | Pas d'impact direct — condition du pilotage | Trivial | Nul | Bug reproduit sur 30/30 runs |
| 3 | **Noyau allégé** : `make virtconfig` + fragment virtio (retirer drivers physiques), compression LZ4 ; bzImage 14 Mo → ~4-6 Mo | Plancher −50 à −150 ms (p95 suit) ; moins d'initcalls/VM exits | Moyen (rebuild + revalidation sha256) | Faible (config déjà pilotée par fragment) | Printk console ; pratique Firecracker (noyau minimal, boot kernel ~120 ms) |
| 4 | **Cmdline** : `nowatchdog nmi_watchdog=0 tsc=reliable random.trust_cpu=on` (+ `console=null` en prod, garder `quiet` — jamais bavard) | Modéré sur plancher et queue (exits NMI/perf, lectures d'horloge émulée) | Faible | Faible (perte de debug console en prod uniquement) | Console bavarde : runs ×3 plus lents |
| 5 | **Affinité vCPU/VMM** (`--cpus affinity=0@N` v54, `taskset` du VMM sur cœur fixe ≠ HT-sibling du vCPU) | Faible sur la queue (causée par ARP) ; peut resserrer le corps | Moyen | Faible | `taskset 2,3` : queue inchangée ; à retester après fix ARP |
| 6 | `--memory prefault=on / hugepages=on` | ~Nul ici (aucune compaction mesurée) | Faible | Mémoire hôte ↑, latence plancher possible ↑ | vmstat propre ; dépriorisé |
| 7 | Initramfs plus petit (`strip benchhttp` 4,8 Mo statique, `opt-level=z`, cpio.gz) | −10 à −30 ms | Faible | Faible | Tailles fichiers |
| 8 | Re-mesure sur **KVM natif** | Les exits imbriqués amplifient plancher et console ; la cause ARP, elle, est hyperviseur-indépendante | Moyen | — | Déjà prévu par le doc |

## 6. Plan d'action (top 5)

1. **Poser l'entrée ARP permanente dans `launch()`** juste après le spawn de
   cloud-hypervisor (attendre `ip link show myc0`, puis `ip neigh replace …
   nud permanent`) + garder la variante `arping -U` côté invité comme
   ceinture de sécurité.
2. **Corriger le bug `wait_ready`** et instrumenter deux chronomètres :
   T_boot-kernel (dernier printk console) et T_premier-contact (TCP).
3. **Rejouer la campagne ABBA** (n ≥ 100 par série, warmups ≥ 5, machine
   tassée) : attendu p50 ~345–380 ms, p95 < 500 ms, CV < 10 % — la comparaison
   avec Docker devrait devenir un quasi-égalité jusqu'à la queue.
4. **Noyau `virtconfig` allégé + LZ4 + cmdline** (`nowatchdog
   nmi_watchdog=0 tsc=reliable random.trust_cpu=on`), `console=null` en prod.
5. **Re-tester affinité vCPU / prefault** sur le nouveau plancher (n = 100,
   critère p95), puis re-mesure sur KVM natif pour quantifier la taxe WSL2.

## Annexe — reproductibilité

- Analyse : `awk` pur (aucune dépendance), scripts et sorties archivés dans
  `/home/nboukadoum-ext/build/` (an_*.sh, boot30/ : consoles horodatées,
  journaux neigh, résultats appariés/taskset/arpfix).
- L'aide `cloud-hypervisor --help` a été capturée avec `timeout 3` +
  redirection (`build/ch_help.txt`) : v54.0 confirme `--cpus
  affinity=<list_of_vcpus_with_their_associated_cpuset>`, `--memory
  prefault|thp|hugepages`, `--platform`, `--net offload_tso/ufo/csum`.
- Hypothèses écartées avec preuves : steal (`/proc/stat` = 0 sur tous les
  runs), THP/compaction (deltas vmstat = 0), fréquences CPU (figées/cosmétiques
  sous WSL2), sonde curl (test apparié), périodicité (intervalles de tails
  non périodiques).
