# Veille scientifique & ingénierie — Déduplication par blocs, snapshot/restore de microVM, latence de queue

> Livrable de recherche fondamentale pour Mycel (runtime de conteneurs
> content-addressed). Établi le 2026-11-03 par veille web (USENIX/ACM/IEEE,
> docs officielles Firecracker/Cloud Hypervisor, blogs d'ingénierie AWS).
> Contexte interne : store v1 fichier-entier BLAKE3
> ([chunk-prototype.md](chunk-prototype.md)), tier `kvm-microvm` Cloud
> Hypervisor v54 + libkrun v1.19.7 sous WSL2-KVM
> ([isolation-engines.md](../isolation-engines.md)).
>
> Conventions : chaque affirmation chiffrée porte sa référence `[Refnn]`,
> résolues en bibliographie. Les mesures internes Mycel sont signalées
> « (mesure Mycel) ».

---

## Erratum important (corrections par rapport au cadrage initial)

La vérification systématique des références a corrigé plusieurs attributions :

| Cadrage initial | Vérifié |
|---|---|
| FastCDC — FAST'16 | **USENIX ATC 2016** [Ref08] |
| Catalyzer — OSDI'20 | **ASPLOS 2020** [Ref19] |
| SEUSS — ATC'23 | **EuroSys 2020** [Ref21] |
| REAP — EuroSys'23 | **ASPLOS 2021** [Ref20] |
| « Snowfork » EuroSys'23 | Introuvable ; l'article fondateur du VM fork est **SnowFlock, EuroSys 2009** [Ref23] |
| « Nize (SIGMOD'21) » / « NizeCDC » | **Introuvable** (arXiv, DBLP, Semantic Scholar, web) ; les descendants vérifiés de FastCDC sont AE, RAM, SS-CDC, Hashless CDC, UltraCDC, MinCDC [Ref09][Ref10] |
| « Letter » | Introuvable en tant qu'algorithme CDC ; probable confusion avec « Better/Min CDC » [Ref12] |
| Leap-Based CDC — 2022 | **MSST 2015** [Ref11] (un successeur 2023 existe : « speculative jump », IEEE TDSC) |
| Firecracker « 120 ms » | La revendication officielle est « **< 125 ms** » (NSDI'20 [Ref16], site officiel) |

---

# A) Déduplication par blocs (content-defined chunking)

## A.1 Fondamentaux et généalogie des algorithmes

Le *Rabin fingerprinting* (polynôme irréductible sur GF(2), calcul glissant)
date de 1981 [Ref01], mais le découpage *défini par le contenu* (CDC) n'a été
inventé qu'en l'appliquant comme rolling hash pour poser des frontières de
blocs : Manber 1993 pour la similarité de fichiers, puis **LBFS** (SOSP 2001,
fenêtre Rabin de 48 octets, masque de bits bas) qui a popularisé la technique
[Ref02]. **Venti** (FAST 2002) est le pionnier du stockage
content-addressed mais à **taille fixe** [Ref03].

Le CDC résout le problème du *shift* : une insertion d'octet au milieu d'un
fichier décale toutes les frontières en taille fixe (Venti) mais seulement
1-2 frontières en CDC (propriété d'« unshift-stability », démontrée sur nos
propres mesures [Ref04]).

### Tableau 1 — Algorithmes de chunking

| Algorithme | Principe | Débit (mesuré indépendamment, cible 2 KiB) | Ratio dédup vs Rabin | Réf. |
|---|---|---|---|---|
| Rabin (BSW) | fingerprint polynomial glissant 32 o | ~202 MiB/s scalaire | référence (meilleur ratio, variance élevée σ≈μ) | [Ref05][Ref09] |
| Buzhash (bup) | table de substitution + rotations, 64 o | ~338 MiB/s | ≈ Rabin (< 0,01 pt d'écart) | [Ref05][Ref09] |
| Gear | `h=(h<<1)+GEAR[b]`, fenêtre implicite | ~599 MiB/s scalaire 32 bits ; **~941-1024 MiB/s SIMD AVX2** (crate `gearhash`) | ≈ Rabin, top tier | [Ref05][Ref09][Ref10] |
| FastCDC | Gear + **normalisation** (2 masques : court avant le centre, long après) + skip du minimum | ≈ 3× Rabin-CDC pour un ratio quasi identique (±1-2 %) | équiv. | [Ref08] |
| AE | extremum asymétrique (min fenêtre) | ~734 MiB/s | compétitif ; formule originale `h=μ/(e-1)` **erronée** (39 % de blocs trop petits à 8 KiB) — corrigée `h≈μ-256` | [Ref09][Ref24] |
| RAM | extremum rapide | ~887 MiB/s (scalaire le plus rapide) | dégénère sur données réelles (moyenne 250 Ko pour cible 8 KiB sur CODE) | [Ref09][Ref25] |
| Leap-based | saut de M octets après une coupe (probabilités P0/Pw) | ≥ FastCDC à l'époque | équiv. | [Ref11] |
| SS-CDC | 2 étapes parallèles | ×25,6 | sans perte | [Ref26] |
| Hashless/SeqCDC (2024) | sans hash par octet | 4 000-4 250 MB/s à 8 Ko (~1,5× FastCDC) | comparable | [Ref10] |
| UltraCDC (Denis) | extremum + heuristiques | ~2,7 GB/s (impl. Zig) | comparable | [Ref27] |
| MinCDC (Peters, 2025) | minimum local de fenêtre + SIMD | très élevé (SIMD natif) | comparable | [Ref12] |
| Taille fixe (FSC) | plané | ~2 529 MiB/s | **suffisant pour images disque de VM** (Jin et al. SYSTOR'09) | [Ref09][Ref13] |

Lecture pour Mycel : le couple **Gear + normalisation FastCDC** est le
meilleur compromis vérifié — c'est exactement le design du prototype
`myc-chunk` [Ref04]. Les dérivés récents (UltraCDC, MinCDC, Hashless) gagnent
en débit mais pas en ratio ; notre goulot n'est pas le chunking (débit
Go/s) mais la gestion du catalogue de blocs (cf. Data Domain [Ref07]).

## A.2 Systèmes réels : choix de granularité

### Tableau 2 — Réglages de systèmes en production

| Système | Chunking | Min / cible / max | Remarque | Réf. |
|---|---|---|---|---|
| LBFS | Rabin 48 o | ~1-2 Ko de cible (13 bits) | origine du CDC, objectif bande-passante réseau | [Ref02] |
| bup | Buzhash-like (bupsplit) | ~128 Ko (fenêtre 21 bits) | packfiles git ; ratio fin mais index lourds | [Ref05] |
| restic | Rabin | **512 o / 512 KiB / 8 MiB** | compromis orienté sauvegarde ; ~60-80 % de gain typique annoncé | [Ref06] |
| Borg | Buzhash | ~1 KiB / ~512 KiB / 6 MiB (`--chunker-params`) | chunks plus fins que restic → meilleur ratio, index plus gros | [Ref06] |
| Venti / zbackup / casync | fixe / Rabin | 8 Ko (Venti) ; casync: par-fichier + store de chunks | casync = le plus proche de Mycel (CAS + blobs) | [Ref03][Ref28] |
| Data Domain | Rabin + SISL | 4-8 Ko (segments) | inline + **compression locale par conteneur** après dédup | [Ref07] |
| ZFS dedup | fixe (bloc zdb, 128 KiB par défaut) à la écriture | — | réputé gourmand en RAM (index en mémoire) ; CDC à l'étude (issue OpenZFS #11400) | [Ref28] |
| git packfiles | fixe + **delta compression** (libxdiff) | — | le delta compense l'absence de CDC | [Ref28] |

Contribution Data Domain (FAST'08) directement transposable : le goulot d'un
store dédupliqué n'est pas le CPU de chunking mais **l'index disque** —
contourné par *summary vector* (filtre de Bloom : 99 %+ des accès disque
d'index évités), *sparse indexing* (localité) et *compression locale* après
dédup ; résultat : 90 % d'utilisation CPU, système borné CPU et non disque
[Ref07].

## A.3 Tailles de chunks : ce que dit la littérature

- **Plus petit = meilleur ratio, coût marginal** : SYSTOR'09 mesure sur images
  disque que réduire la taille (4 KiB → 512 o) augmente régulièrement le
  ratio, notamment en convertissant des blocs « uniques » en blocs partagés
  et en blocs de zéros [Ref13]. Le survey 2024 confirme sur quatre datasets
  (Rabin : 87,1 % à 512 o sur CODE) [Ref09].
- **FastCDC évalue avec 2 KiB min / 8 Ko cible / 64 KiB max** [Ref08] ;
  la version journal (TPDS 2020) recommande explicitement la normalisation
  pour contenir la variance (σ≈μ sans, fortement réduite avec) [Ref10].
- **Surcoût du manifeste** : ~40 o/chunk dans notre prototype → ~1 % du
  volume à cible 4 Ko (mesure Mycel [Ref04]) ; Meister et al. (FAST'13)
  montrent que les *file recipes* se compriment jusqu'à 93 % [Ref29].
- **Règle pratique observée** : 2-8 Ko pour contenus versionnés fins
  (objets « source »), 64 KiB+ pour gros blobs orientés sauvegarde (restic/
  borg), taille fixe acceptable pour images disque de VM [Ref13].

## A.4 Gain bloc-vs-fichier : chiffres mesurés

### Tableau 3 — Ratios de déduplication rapportés

| Dataset / contexte | Niveau fichier | Niveau bloc | Réf. |
|---|---|---|---|
| 857 postes de travail Microsoft (stockage primaire, blocs 8 Ko, fixe vs Rabin) | — | ~68 % de réduction ; « whole-file dedup + sparseness » suffit pour la majorité des doublons | [Ref14] |
| HPC (1 212 To, SC'12) — *probable source de la référence « HDFS » du cadrage* | — | 20-30 % typique, pics ~70 % (vs 68 %+ en sauvegarde entreprise) | [Ref15] |
| Images disque VM, versions successives d'une même distro (Ubuntu 6.10→8.04) | — | les données stockées « croissent très lentement après les premières images » ; releases non consécutives ≈ consécutives | [Ref13] |
| Même VM sur deux VMM (VMware vs VirtualBox), CDC | — | « ~moitié de l'espace » économisée ; taille fixe échoue (décalage d'en-tête, sauf 512 o) | [Ref13] |
| Forks de distributions (Debian vs Ubuntu vs Knoppix…) | — | **faible** — « il faut les considérer comme distinctes » | [Ref13] |
| Locales en|fr d'une même distro | — | ajoute « très peu de chunks uniques » | [Ref13] |
| LiveDFS, cloud OpenNebula (VM images, chunk) | — | ≥ 40 % d'espace VM | [Ref30] |
| Arbres de snapshots Firecracker incrémentaux (production AWS) | — | réduction « jusqu'à 90 % » du volume transféré (chunks kernel/runtime partagés) | [Ref17] |
| binaires reconstruits (musl, libcrypto, busybox entre alpine 3.20↔3.21) | 0 % (mesure Mycel) | 1,9 % (busybox) à 33 % (musl) — **le CDC ne fait pas de miracle sur des builds non reproductibles** | [Ref04] |
| texte édité (10 lignes / 100 Ko) | 0 % (mesure Mycel) | **98 %** | [Ref04] |

Le verdict nuancé de SYSTOR'09 — *« fixed-size chunking may be appropriate
for VM disk images »* [Ref13] — ne s'applique qu'aux images disque brutes
(alignées) ; dès qu'un préfixe décale le contenu (tarballs, en-têtes OCI,
éditeurs), le CDC reprend l'avantage.

## A.5 Delta encoding en complément

| Outil / technique | Base scientifique | Quand ça bat la dédup | Réf. |
|---|---|---|---|
| rsync (rolling checksum + hachage de blocs) | thèse Tridgell 1999 (ANU), TR 1996 | synchronisation 1-à-1 sur lien lent ; ne généralise pas à un store N-à-N | [Ref31] |
| xdelta3 / VCDIFF | RFC 3284 (2002), lui-même fondé sur Bentley & McIlroy « long common strings » (DCC'99) | deux versions **connues par avance** d'un même artefact : delta quasi minimal | [Ref32] |
| zstd `--train` (dictionnaire partagé) | Collet et al., zstd | petits fichiers répétitifs : compresse des entrées de quelques Ko comme si elles étaient grosses | [Ref33] |
| delta + dédup + compression (Difference Engine, OSDI'08) | Gupta et al. | pages mémoire de VM similaires : le delta capture ce que le chunking rate | [Ref34] |

Positionnement : pour Mycel, le delta n'est pertinent que pour des paires
d'objets « parents » explicites (rebuild d'un même binaire, versions de
manifeste) ; pour un store généraliste, CDC + compression locale par pack
(l'approche Data Domain [Ref07]) domine.

---

# B) Snapshot/restore de microVM (démarrage quasi gratuit)

## B.1 État de l'art industriel

**Firecracker** (NSDI'20) revendique un boot < 125 ms, < 5 MiB de surcoût
mémoire et 150 microVM créées/s/hôte — et admet que « les 125 ms ne suffisent
pas pour le scale-up de Lambda », d'où les snapshots [Ref16]. Le support
snapshot (docs officielles) sérialise **mémoire invité + état des devices +
état KVM/vCPU** ; le restore mappe le fichier mémoire en `MAP_PRIVATE`
(chargement à la demande par le noyau) ou via un backend **userfaultfd** ;
`LoadSnapshot` n'est autorisé qu'avant boot ; cgroups **v2** requis (v1 =
restore lent) ; horloge reprise au moment du snapshot (kvm-clock avançable) ;
VMGenID réamorcé à la reprise ; **reprendre le même snapshot plusieurs fois
est déclaré non sûr** (identifiants, entropie, TCP) [Ref35]. Brooker (AWS)
chiffre le restore à **« aussi peu que 4 ms, ~10 ms pour un Linux complet »**,
et les arbres de snapshots incrémentaux partagent les chunks kernel/runtime
(-90 % de données déplacées) [Ref17] ; le problème d'unicité (PRNG, clés,
UUID, séquences TCP) est traité dans [Ref36].

**AWS Lambda SnapStart** (2022) : snapshot Firecracker (mémoire + disque)
chiffré KMS pris à la publication de version ; **jusqu'à 10× plus rapide**
pour le cold start Java [Ref37] ; en 2025, l'« invoke priming » amène le
p99.9 à 781,68 ms (1,8× mieux que SnapStart seul) [Ref38].

## B.2 Travaux académiques

### Tableau 4 — Snapshot/restore & fork : contributions clés

| Système (venue, année) | Idée | Chiffres clés | Réf. |
|---|---|---|---|
| SnowFlock (EuroSys'09) | **VM fork** : clonage instantané d'une VM, mémoire répliquée à la demande (multicast EPT) | fork « sous la seconde », distribution de pages paresseuse | [Ref23] |
| Catalyzer (ASPLOS'20, SJTU IPADS) | restore **à la demande** (mémoire *et* état système) + primitive **sfork** (fork d'un sandbox initialisé) | 0,97 ms (C-hello) ; Zygote : 5 ms C, 14 ms Java, 9 ms Python, 12 ms Ruby, 9 ms Node ; Java sfork 1,5-2 ms ; 35-67× sur DeathStar ; e2e 4,1-6,5× ; overlay memory : -261 ms sur SPECjbb | [Ref19] |
| SEUSS (EuroSys'20, BU) | unikernels empilés « snapshot stacks », chemins d'init redondants sautés | partage mémoire entre snapshots, forte densité de cache | [Ref21] |
| REAP (ASPLOS'21, Edinburgh) | **record-and-replay** : trace des pages touchées au cold start, prefetch avant exécution | cold start ÷3,7 ; empreinte mémoire -61 à -96 % | [Ref20] |
| On-demand-fork (EuroSys'21) | fork microseconde via mmap CoW | fork de process mémoire-intensifs en µs | [Ref39] |
| HyperFork (2020, Harvard) | clone flash de VM pré-bootées (KVM) | serverless sub-ms | [Ref40] |
| RF-Fork (OSDI'23) | fork **distribué** par RDMA | µs, conteneurs distants | [Ref41] |
| FaaSNet (ATC'21, GMU+Alibaba) | distribution de snapshots par arbre P2P (« Function Tree ») | **2 000 instances provisionnées en ~200 ms** | [Ref22] |
| FaaSnap (EuroSys'22, UCSD) | snapshots de VM Firecracker pour FaaS, prefetch noyau | cold starts quasi éliminés | [Ref42] |
| REWIND (ATC'24) / SnapStart à grande échelle (ATC'24) | snapshot « function-aware » / analyse de SnapStart intégré au VMM | — | [Ref43] |
| Sabre (OSDI'24) | compression de snapshots accélérée matériellement | — | [Ref43] |

Le levier commun : **l'initialisation est un coût, pas une nécessité** — la
plupart des latences viennent du chargement/décodage d'objets système
(gVisor-restore : 400 ms dont 135,9 ms de recovery noyau et 128,8 ms de
mémoire pour SPECjbb ; 37 838 objets restaurés [Ref19]).

## B.3 Techniques mémoire (le cœur du problème)

1. **Restore paresseux par userfaultfd** : les pages ne sont lues qu'au
   premier défaut. Implémenté chez Firecracker (backend `Uffd` [Ref35]),
   Cloud Hypervisor (`memory_restore_mode=ondemand`, strict, exige
   `/dev/userfaultfd` ou `vm.unprivileged_userfaultfd=1` [Ref44]) et CRIU
   (`--lazy-pages`, prefetch optionnel ; en post-copy chaque page n'est
   copiée qu'une fois [Ref45]).
2. **Mapping CoW du fichier snapshot** : zéro copie up-front, les VM issues
   d'un même snapshot partagent le page cache, les écritures restent privées
   — `MAP_PRIVATE` Firecracker [Ref35], `copyonwrite` de Cloud Hypervisor
   (re-snapshot par reflinks sur XFS/btrfs) [Ref44].
3. **Prefetch adaptatif des pages chaudes** : tracer les défauts de page d'un
   restore précédent et précharger ces pages (REAP ÷3,7 [Ref20] ; le
   « prefault » de CRIU lazy-pages [Ref45]).
4. **Compression du snapshot** : gzip/zstd du fichier mémoire (Catalyzer
   compresse les données sauvegardées [Ref19]) ; Sabre la déplace vers le
   matériel [Ref43] ; Firecracker écrit les snapshots différentiels
   (dirty pages uniquement, mode preview) [Ref35].
5. **NVDIMM/PNM** : la persistance de pages en mémoire non volatile
   supprimerait la lecture disque du restore, mais aucun des environnements
   Mycel (WSL2, KVM nuage) n'expose de NVDIMM à l'invité — piste non
   actionnable à court terme, mentionnée pour exhaustivité.

## B.4 Application à notre stack (constats mesurés 2026-11-03)

| Brique | Capacité snapshot/restore | Conséquence |
|---|---|---|
| Cloud Hypervisor v54 | `pause → snapshot → restore → resume` natif ; 3 fichiers (config.json, memory-ranges, state.json) ; modes mémoire `copy`/`ondemand`(uffd)/`copyonwrite` ; VMGenID présent | le seul chemin complet dont nous disposons [Ref44] |
| libkrun v1.19.7 | **aucune API** (vérifié dans `include/libkrun.h`) (mesure Mycel [Ref46]) | pas de S/R côté libkrun ; fork uniquement |
| WSL2-KVM (nested) | snapshot ~0,3 s / 512 MiB, restore charge la mémoire et relance le vCPU, **mais l'invité reste inerte** : ni timer ni interruptions (3 modes mémoire testés) ; MSR/état vCPU non préservés par le KVM de WSL2 (mesure Mycel [Ref46]) | le S/R y est inutilisable en l'état |

Le constat WSL2 rejoint la documentation : les snapshots Firecracker doivent
être repris sur **matériel/logiciel identiques** (le docs cite explicitement
des MSR non préservées au restore [Ref35]), et l'VM utilitaire WSL2 n'expose
ni checkpoints ni migration [Ref47], avec un L2 « non éclairé » (les
enlightenments Hyper-V ne traversent pas les couches) [Ref47].

### Approches qui contournent un KVM hôte pauvre

1. **Chaude piscine + fork léger** (SnowFlock/Catalyzer/HyperFork
   [Ref23][Ref19][Ref40]) : booter UNE fois, puis cloner à la demande — le
   clonage n'exige pas de sérialiser l'état vCPU, seulement un mmap CoW de
   la mémoire ; c'est la voie robuste face à un KVM qui perd les MSR.
2. **Snapshot applicatif dans l'invité** (SEUSS/REAP/FaaSnap [Ref21][Ref20]
   [Ref42]) : le restore se fait au niveau de l'application/fichier, pas du
   VMM — insensible aux défauts de KVM hôte. Mycel a déjà l'équivalent
   « filesystem » : le store hardlink + lazy FUSE est un snapshot applicatif
   à 190 ms cold (mesure Mycel, README).
3. **Fast-boot kernel au lieu de restore** : kernel élagué + initramfs
   intégré (notre `bench/microvm`, Linux 6.12 piné, virtio built-in) —
   l'ordre de grandeur Firecracker (125 ms [Ref16]) devient accessible sans
   S/R du tout.
4. **Re-validation sur KVM natif** avant d'investir (déjà planifié dans
   isolation-engines.md [Ref46]) — les limites observées sont des limites
   WSL2, pas des limites Cloud Hypervisor.

---

# C) Latence de queue sous virtualisation imbriquée (WSL2/KVM)

## C.1 Le problème de la queue

Dean & Barroso (« The Tail at Scale », CACM 56(2):74-80, 2013) : si chaque
composant a 1 % de risque d'être lent, une requête touchant 100 serveurs
dépassera 1 s dans **63 %** des cas ; les sources de variabilité incluent le
partage de ressources, les démons d'arrière-plan, le GC ; remèdes : requêtes
couvertes (« hedged »), mise en probation des lenteurs, gestion fine des
files [Ref48]. Sous virtualisation **imbriquée** (L0 Hyper-V → L1 WSL2-KVM →
L2 microVM), chaque couche ajoute ses propres files : le fondement théorique
est le projet Turtles (OSDI'10) — chaque world-switch de L1 est piégé et
émulé par L0 [Ref49] ; les surcoûts typiques sont de 10-30 % par couche
d'imbrication (mesures Red Hat « Inception ») [Ref50], et HyperTurtle
(ATC'25) documente le coût des world-switchs imbriqués [Ref51]. Les
enlightenments Hyper-V (timers synthétiques, TSC sync) ne passent pas au L2
[Ref47], et l'VM utilitaire WSL2 n'offre ni checkpoint ni migration [Ref47].

## C.2 Sources de jitter mesurables et leviers

| Source | Effet sur p95 | Levier documenté | Réf. |
|---|---|---|---|
| vol de vCPU / steal time | descheduled → spikes | **pinning** vCPU→pCPU + cgroups/isolation ; le steal time se mesure dans l'invité | [Ref52][Ref53] |
| ticks noyau & RCU sur les cœurs VM | interruptions périodiques | `isolcpus`/cpuset, `nohz_full=`, `rcu_nocbs=` sur les cœurs dédiés (limite : peu contrôlable dans WSL2) | [Ref53][Ref54] |
| exit/entry virtio & réveils | latence d'attente IRQ | vhost/vhost-user (dataplane noyau), **halt-polling** KVM, spin court avant sommeil | [Ref55][Ref56] |
| timers paravirt | dérive/jitter d'horloge | kvm-clock / clocksource=tsc ; cmdline microVM minimale | [Ref16][Ref57] |
| EPT/MLP imbriqué (shadow→2D) | coûts de défaut EPT L2 | réduire la surface mémoire (initramfs), prefault/populate | [Ref49][Ref51] |
| PML (dirty logging) | Catalyzer mesure 5-8 ms par `KVM_SET_MEMORY_REGION` tant que PML est actif | désactiver le dirty-page tracking quand on ne snapshotte pas | [Ref19] |
| allocations kvm (kvcalloc) | 1,6 ms par boot (mesuré Catalyzer) | cache dédié côté VMM — à imiter dans myc-backend | [Ref19] |
| syscalls à queue haute (dup/dup2, expansion fdtable) | 1-30 ms observés | lazy-dup / prédimensionner les fdtables | [Ref19] |
| cgroups v1 | restore lent (docs Firecracker) | imposer cgroups v2 | [Ref35] |

## C.3 Recommandations boot microVM (kernel cmdline & flags)

Baseline officielle Firecracker :
`reboot=k panic=1 nomodule 8250.nr_uarts=0 i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd`
[Ref57]. Compléments issus de la pratique mesurée [Ref58][Ref59] :

- `quiet` + `initcall_blacklist=` des pilotes lents identifiés par
  `initcall_debug` (chaque printk console virtuelle coûte des ms) ;
- drivers **built-in** (pas de modules, pas d'udev), initramfs minuscule ou
  kernel avec rootfs intégré — notre `bench/microvm` suit déjà cette voie ;
- pas de systemd (ou systemd réduit) : ~200-300 ms boot-to-systemd observés
  sur Firecracker optimisé [Ref58] ; init statique (busybox) comme au bench ;
- CPU : vCPUs pinés quand l'hôte le permet, topologie exposée honnête
  (`-c topologia` Cloud Hypervisor), pas d'hyperthread croisé ;
- mémoire : dimensionner juste + prefault/MAP_POPULATE côté VMM pour
  déplacer les défauts hors de la mesure p95 (compromis avec le boot) ;
- mesurer **toujours** en p95 médiané sur ≥30 runs avec protocole ABBA —
  c'est déjà le protocole de `bench/microvm` (mesure Mycel).

---

# Recommandations pour Mycel

## R1 — Chunking : garder FastCDC/Gear, deux profils, chunks conditionnels

**Algorithme : Gear 64 bits + normalisation FastCDC** (comme le prototype
[Ref04]) : ~1 GiB/s SIMD mesuré [Ref09], ratio ≈ Rabin [Ref08][Ref09],
aucun `unsafe`, déterminisme inter-machines. UltraCDC/MinCDC ne s'imposent
que si le débit devient le goulot (ingestion multi-Go/s).

**Tailles** (justifiées §A.3) :

| Paramètre | Fichiers < 64 KiB | Fichiers ≥ 64 KiB « source » | Gros blobs (images disque, snapshots mémoire) |
|---|---|---|---|
| Politique | **pas de chunking** (fichier-entier reste l'unité) | min 2 KiB / cible 4 KiB / max 16 KiB (profil prototype) | min 16 KiB / cible **64 KiB** / max 256 KiB |

Justification : Meyer & Bolosky montrent que whole-file + sparseness couvre
la majorité des doublons de postes de travail [Ref14] ; le chunking fin paie
sur objets texte/versionnés (98 % mesurés [Ref04]) mais coûte ~1 % de
manifeste et du fanout store [Ref04][Ref29] ; 64 KiB est la borne haute
d'évaluation de FastCDC [Ref08] et correspond à la granularité utile des
images disque/blobs mémoire (le SYSTOR'09 montre qu'au-delà le ratio ne
s'améliore plus assez pour payer l'index [Ref13]).

**Store de chunks façon Data Domain** : regrouper les chunks dans des
*packs* compressés localement (zstd) avec filtre de Bloom en avant-poste de
l'index — 99 %+ des recherches servies sans toucher le disque [Ref07] ;
penser **refcounting par chunk** et fusion du dernier bloc court dès la v2
(limites déjà identifiées [Ref04]).

## R2 — Snapshot/restore : fork paresseux d'abord, S/R complet sur KVM natif seulement

1. **Court terme (WSL2)** : ne pas construire sur le restore vCPU (invité
   inert, mesure Mycel [Ref46]). Utiliser le **modèle chaude-piscine/fork**
   (SnowFlock [Ref23], sfork Catalyzer [Ref19], HyperFork [Ref40]) :
   boot-once → clone CoW mémoire. En pratique Mycel : process pool de VMs
   pré-bootées dans `myc-backend`, « run » = mmap CoW + exec dans l'invité.
2. **Moyen terme (KVM natif)** : ré-activer le chemin Cloud Hypervisor v54
   `copyonwrite`/`ondemand` [Ref44] (le copyonwrite est le plus robuste :
   zéro copie, reflinks au re-snapshot) + **prefetch des pages chaudes à la
   REAP** (trace des défauts du 1er restore, ÷3,7 sur cold start [Ref20]).
3. **Toujours** : VMGenID + ré-amorçage d'entropie à la reprise (exigence
   documentée Firecracker [Ref35], unicité traitée dans [Ref36]), reprise
   unique par snapshot, et chiffrer les snapshots au repos (comme SnapStart
   KMS [Ref37]).
4. **Libkrun** : aucune brique S/R à attendre de v1.19.7 (mesure Mycel
   [Ref46]) ; y baser le fork nécessiterait d'ajouter l'API en amont.

## R3 — Latence p95 du boot microVM : top 5 actions

1. **Réduire le chemin de boot invité** : cmdline minimale à la Firecracker
   (`reboot=k panic=1 nomodule 8250.nr_uarts=0 i8042.*` [Ref57]) + `quiet`,
   `initcall_blacklist` ciblé, drivers built-in, init statique — l'écart
   mesuré entre kernel générique et kernel élagué est de plusieurs
   centaines de ms [Ref58][Ref59].
2. **Éviter le jitter vCPU** : pinner vCPU et threads VMM quand l'hôte le
   permet (KVM natif), mesurer le steal time en continu ; sous WSL2,
   limiter les cœurs exposés à la VM pour réduire la contention avec
   vmmem [Ref52][Ref50].
3. **Désactiver le dirty-page tracking/PML hors des snapshots** : 5-8 ms
   économisés par setup de région mémoire KVM (mesure Catalyzer [Ref19]).
4. **Prefaulter la mémoire invité au boot** (ou `MAP_POPULATE`/`prefault=on`
   Cloud Hypervisor) pour payer les défauts EPT hors du chemin mesuré,
   quitte à les remettre en mode paresseux pour le restore [Ref44].
5. **Virtio efficace + halt-polling** : devices virtio-mmio built-in,
   dataplane vhost côté hôte quand disponible, `halt_polling` ajusté pour
   réduire les réveils IRQ (les exits sont la première source de queue sous
   virt imbriquée) [Ref55][Ref56][Ref51].

Et transversal : garder le protocole de mesure p95/ABBA de `bench/microvm`
(médiane + p95 sur 30 runs, fuite = échec) — c'est exactement la discipline
que « The Tail at Scale » prescrit pour ne pas optimiser la moyenne pendant
que la queue explose [Ref48].

---

# Bibliographie

### Sujet A — Chunking & déduplication

- **[Ref01]** M. O. Rabin, *Fingerprinting by Random Polynomials*, Technical
  Report TR-15-81, Aiken Computation Lab., Harvard University, 1981.
  http://www.xmailserver.org/rabin.pdf
- **[Ref02]** A. Muthitacharoen, B. Chen, D. Mazières, *A Low-Bandwidth
  Network File System (LBFS)*, SOSP 2001.
  https://dl.acm.org/doi/10.1145/502034.502043
- **[Ref03]** S. Quinlan, S. Dorward, *Venti: A New Approach to Archival Data
  Storage*, USENIX FAST 2002.
  https://www.usenix.org/publications/library/proceedings/fast02/quinlan/quinlan.pdf
- **[Ref04]** N. Boukadoum, *Prototype : déduplication par blocs
  (content-defined chunking)* — docs/research/chunk-prototype.md, repo Mycel,
  2026 (mesures internes alpine 3.20/3.21).
- **[Ref05]** restic blog, *Introducing Content Defined Chunking*, 2015 ;
  docs Borg 1.1 (buzhash, chunker-params) ; docs rustic (Rabin restic
  512 KiB / bup ~128 KiB). https://restic.net/blog/2015-09-12/restic-foundation1-cdc ;
  https://borgbackup.readthedocs.io/en/1.1-maint ; https://rustic.cli.rs/dev-docs/design/backups_deduplication.html
- **[Ref06]** Comparatifs BorgBackup/restic/bup (forum restic « Why am I
  seeing such poor dedup'ing », 2022 ; Onidel 2025).
  https://forum.restic.net/t/why-am-i-seeing-such-poor-deduping/5334
- **[Ref07]** B. Zhu, K. Li, H. Patterson, *Avoiding the Disk Bottleneck in
  the Data Domain Deduplication File System*, USENIX FAST 2008.
  https://www.usenix.org/legacy/event/fast08/tech/full_papers/zhu/zhu.pdf
- **[Ref08]** W. Xia, H. Jiang, D. Feng, L. Tian, M. Fu, Y. Zhou, *FastCDC:
  A Fast and Efficient Content-Defined Chunking Approach for Data
  Deduplication*, USENIX ATC 2016, pp. 101-114.
  https://www.usenix.org/conference/atc16/technical-sessions/presentation/xia
- **[Ref09]** M. Gregoriadis, J. Pouwelse (TU Delft), L. Balduf,
  B. Scheuermann (TU Darmstadt), *A Thorough Investigation of
  Content-Defined Chunking Algorithms for Data Deduplication*, arXiv
  2409.06066, 2024 (débits : Rabin ~202 MiB/s, Gear SIMD ~1 GiB/s ;
  formule corrigée d'AE `h≈μ-256` ; dégenerescence de RAM).
  https://arxiv.org/abs/2409.06066
- **[Ref10]** W. Xia, X. Zou, H. Jiang, Y. Zhou, C. Liu, D. Feng, Y. Hua,
  Y. Hu, Y. Zhang, *The Design of Fast Content-Defined Chunking for Data
  Deduplication-Based Storage Systems*, IEEE TPDS 31(9):2017-2031, 2020 ;
  et *Hashless Content-Defined Chunking* (ACM, 2024, SeqCDC 4-4,25 GB/s à
  8 KiB). https://ranger.uta.edu/~jiang/publication/Journals/2020/2020-IEEE-TPDS(Wen%20Xia).pdf ;
  https://dl.acm.org/doi/abs/10.1145/3652892.3700766
- **[Ref11]** Y. Li, H. Jiang, et al., *Leap-based Content Defined Chunking —
  Theory and Implementation*, IEEE MSST 2015 ; successeur : X. Jin et al.,
  *Accelerating CDC Based on Speculative Jump*, IEEE TDSC 2023.
  https://www.researchgate.net/publication/308846672
- **[Ref12]** O. Peters, *MinCDC*, 2025 (crate Rust SIMD).
  https://github.com/orlp/mincdc ; voir aussi *The Chonkers Algorithm*,
  arXiv 2509.11121, 2025.
- **[Ref13]** K. Jin, E. L. Miller, D. Patterson, *The Effectiveness of
  Deduplication on Virtual Machine Disk Images*, ACM SYSTOR 2009.
  https://dl.acm.org/doi/10.1145/1534530.1534540
- **[Ref14]** D. T. Meyer, W. J. Bolosky, *A Study of Practical
  Deduplication*, USENIX FAST 2011 (857 postes Microsoft ; ~68 % en stockage
  primaire). https://www.usenix.org/legacy/event/fast11/tech/full_papers/Meyer.pdf
- **[Ref15]** D. Meister, J. Kaiser, A. Brinkmann, T. Cortes, M. Kuhn,
  J. Kunkel, *A Study on Data Deduplication in HPC Storage Systems*, ACM SC
  2012 (1 212 To ; 20-30 % typique, pics ~70 %).
  https://dl.acm.org/doi/10.5555/2388996.2389006
- **[Ref29]** D. Meister, A. Brinkmann, *File Recipe Compression in Data
  Deduplication Systems*, USENIX FAST 2013 (recipes -93 %).
  https://www.usenix.org/conference/fast13/technical-sessions/presentation/meister
- **[Ref30]** B. Ng et al., *Live Deduplication Storage of Virtual Machine
  Images in an Open-Source Cloud (LiveDFS)*, ACM/IFIP/USENIX Middleware
  2011 (≥ 40 % sur VM).
  https://www.cse.cuhk.edu.hk/~cslui/PUBLICATION/middleware11.pdf
- **[Ref31]** A. Tridgell, *Efficient Algorithms for Sorting and
  Synchronization*, PhD thesis, ANU, 1999 (rsync).
  https://www.samba.org/~tridge/phd_thesis.pdf
- **[Ref32]** D. Korn, J. Perl, E. Baentsch, et al., *VCDIFF Generic
  Differencing and Compression Data Format*, IETF RFC 3284, 2002 ;
  xdelta3 : https://github.com/jmacd/xdelta
- **[Ref33]** Y. Collet, E. Turner, zstd (dictionnaires `--train`) ;
  RocksDB, *Preset Dictionary Compression*, 2021.
  https://rocksdb.org/blog/2021/05/31/dictionary-compression.html
- **[Ref34]** D. Gupta, S. Lee, M. Vrable, S. Savage, A. C. Snoeren,
  G. Varghese, G. M. Voelker, A. Vahdat, *Difference Engine: Harnessing
  Memory Redundancy in Virtual Machines*, USENIX OSDI 2008.
  https://www.usenix.org/legacy/event/osdi08/tech/full_papers/gupta/gupta.pdf
- **[Ref27]** F. Denis, *UltraCDC* (Zig, ~2,7 GB/s), 2023.
  https://github.com/jedisct1/zig-ultracdc
- **[Ref28]** OpenZFS issue #11400 (CDC pour dedup) ;
  casync : https://github.com/systemd/casync ; zbackup :
  https://github.com/zbackup/zbackup ; format packfile git :
  https://git-scm.com/docs/pack-format
- **[Ref24]** Y. Zhang, H. Jiang, D. Feng, W. Xia, M. Fu, F. Huang, Y. Zhou,
  *Æ: An Asymmetric Extremum Content Defined Chunking Algorithm for Fast and
  Bandwidth-Efficient Data Deduplication*, IEEE INFOCOM 2015, pp. 1337-1345
  (ext. IEEE Trans. Computers 66(2), 2017). Référencement complet in
  [Ref09].
- **[Ref25]** R. N. Widodo, H. Lim, M. Atiquzzaman, *A New Content-Defined
  Chunking Algorithm for Data Deduplication in Cloud Storage*, FGCS 71:145-156,
  2017.
- **[Ref26]** *SS-CDC: A Two-Stage Parallel Content-Defined Chunking*, ACM
  2019 (×25,6). https://dl.acm.org/doi/10.1145/3319647.3325834

### Sujet B — Snapshot/restore

- **[Ref16]** A. Agache, M. Brooker, A. Florescu, et al., *Firecracker:
  Lightweight Virtualization for Serverless Applications*, USENIX NSDI 2020
  (< 125 ms, < 5 MiB, 150 VM/s). https://www.usenix.org/system/files/nsdi20-paper-agache.pdf
- **[Ref35]** Firecracker docs, *Snapshot support* (MAP_PRIVATE/uffd, cgroups
  v2, VMGenID, hardware identique, insécurité du multi-resume).
  https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md
- **[Ref17]** M. Brooker, *Lambda SnapStart and snapshots as a tool*, 2022
  (restore ~4 ms / ~10 ms ; arbres incrémentaux -90 % de déplacement).
  https://brooker.co.za/blog/2022/11/29/snapstart.html ; et *Seven Years of
  Firecracker*, 2025. https://brooker.co.za/blog/2025/09/18/firecracker.html
- **[Ref36]** M. Brooker, *Restoring Uniqueness in MicroVM Snapshots*,
  arXiv 2102.12892, 2021. https://arxiv.org/abs/2102.12892
- **[Ref37]** AWS Compute Blog, *Reducing Java cold starts on AWS Lambda
  with SnapStart*, 2022 (jusqu'à 10×) ; docs Lambda SnapStart (snapshot
  Firecracker, KMS). https://aws.amazon.com/blogs/compute/reducing-java-cold-starts-on-aws-lambda-functions-with-snapstart/
- **[Ref38]** AWS Compute Blog, *Optimizing cold starts with advanced
  priming strategies + SnapStart*, 2025 (p99.9 781,68 ms, 1,8×).
  https://aws.amazon.com/blogs/compute/optimizing-cold-start-performance-of-aws-lambda-using-advanced-priming-strategies-with-snapstart/
- **[Ref19]** D. Du, T. Yu, Y. Xia, B. Zang, G. Yan, C. Qin, Q. Wu,
  H. Chen, *Catalyzer: Sub-millisecond Startup for Serverless Computing with
  Initialization-less Booting*, ACM ASPLOS 2020 (chiffres extraits du PDF :
  0,97 ms sfork ; 5/14/9/12/9 ms Zygote C/Java/Python/Ruby/Node ; -261 ms
  overlay ; PML -5-8 ms ; kvcalloc 1,6 ms→<50 µs).
  https://ipads.sjtu.edu.cn/_media/publications/catalyzer-asplos20.pdf
- **[Ref20]** D. Ustiugov, B. Pietrov, et al., *Benchmarking, Analysis and
  Optimization of Serverless Function Snapshots (REAP)*, ACM ASPLOS 2021
  (÷3,7 ; mémoire -61-96 %). https://dl.acm.org/doi/10.1145/3445814.3446714
- **[Ref21]** J. Cadden et al., *SEUSS: Skip Redundant Paths to Make
  Serverless Fast*, EuroSys 2020.
  https://www.cs.bu.edu/~jappavoo/Resources/Papers/seuss.pdf
- **[Ref22]** A. Wang et al., *FaaSNet: Scalable and Fast Provisioning of
  Custom Serverless Container Runtimes at Alibaba Cloud FunctionCompute*,
  USENIX ATC 2021 (2 000 instances ~200 ms).
  https://www.usenix.org/conference/atc21/presentation/wang-ao
- **[Ref23]** H. A. Lagar-Cavilla et al., *SnowFlock: Rapid Virtual Machine
  Cloning for Cloud Computing*, EuroSys 2009.
  https://dl.acm.org/doi/10.1145/1519065.1519067
- **[Ref39]** D. Zhao et al., *On-demand-fork: A Microsecond Fork for
  Memory-Intensive and Latency-Sensitive Applications*, EuroSys 2021.
  https://www.cs.purdue.edu/homes/pfonseca/papers/eurosys21-odf.pdf
- **[Ref40]** A. Colavita et al., *HyperFork: lightweight VM cloning for
  serverless*, 2020 (KVM/kvmtool). https://dash.harvard.edu/bitstreams/61828360-35c7-41ef-9ff2-2f5b13bc539e/download
- **[Ref41]** S. Wei et al., *RF-Fork: Fast RDMA-co-designed Remote Fork for
  Serverless*, USENIX OSDI 2023.
  https://www.usenix.org/system/files/osdi23-wei-rdma.pdf
- **[Ref42]** L. Ao et al., *FaaSnap: FaaS Made Fast Using Snapshot-based
  VMs*, EuroSys 2022. https://www.sysnet.ucsd.edu/~voelker/pubs/faasnap-eurosys22.pdf
- **[Ref43]** REWIND, USENIX ATC 2024
  (https://www.usenix.org/conference/atc24/presentation/song) ; Q. Pang et
  al., *Expeditious High-Concurrency MicroVM SnapStart*, USENIX ATC 2024
  (https://www.usenix.org/system/files/atc24-pang.pdf) ; Sabre, OSDI 2024.
- **[Ref44]** Cloud Hypervisor, docs *snapshot_restore.md* (pause/snapshot/
  restore/resume ; copy / ondemand-uffd / copyonwrite ; reflinks XFS/btrfs ;
  VMGenID). https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/snapshot_restore.md
- **[Ref45]** CRIU wiki *Lazy migration* (lazy-pages userfaultfd, prefetch) ;
  *Understanding Migration Mechanisms of Containers using CRIU* (post-copy :
  chaque page copiée une fois). https://criu.org/Lazy_migration
- **[Ref46]** Mycel, *docs/isolation-engines.md — Snapshot/restore status
  (measured, not assumed)*, constats du 2026-11-03 : libkrun v1.19.7 sans
  API snapshot ; CH v54 sous WSL2-KVM : restore → invité inert.
- **[Ref47]** microsoft/WSL issue #12567 (pas de checkpoints pour la VM
  utilitaire) ; Gist *KVM on WSL2* (pas de passthrough, L2 non éclairé) ;
  Microsoft Learn *WSL advanced settings* (`nestedVirtualization`).
  https://github.com/microsoft/WSL/issues/12567

### Sujet C — Latence de queue

- **[Ref48]** J. Dean, L. A. Barroso, *The Tail at Scale*, Communications of
  the ACM 56(2):74-80, 2013 (1 % × 100 serveurs → 63 % > 1 s).
  https://www.barroso.org/publications/TheTailAtScale.pdf
- **[Ref49]** M. Ben-Yehuda et al., *The Turtles Project: Design and
  Implementation of Nested Virtualization*, USENIX OSDI 2010.
  https://www.usenix.org/event/osdi10/tech/full_papers/Ben-Yehuda.pdf
- **[Ref50]** Red Hat, *Inception: How usable are nested KVM guests?*, 2017
  (surcoûts par couche) ; pratique communément rapportée 10-30 %/couche.
- **[Ref51]** N. Zur et al., *Accelerating Nested Virtualization with
  HyperTurtle*, USENIX ATC 2025 (world-switchs imbriqués).
  https://www.usenix.org/conference/atc25/presentation/zur
- **[Ref52]** Red Hat RHEL 9, *Optimizing VM performance* (vCPU pinning) ;
  KVM/steal time (mesure `st` dans l'invité).
  https://docs.redhat.com/en/documentation/red_hat_enterprise_linux/9/html/configuring_and_managing_virtualization/
- **[Ref53]** E. Rigtorp, *Low Latency Guide* (isolcpus, rcu_nocbs, power) ;
  Red Hat RT *Isolating CPUs using tuned-profiles-realtime* (nohz vs
  nohz_full). https://rigtorp.se/low-latency-guide/
- **[Ref54]** Ubuntu Discourse, *Enable low-latency features in the generic
  Ubuntu kernel 24.04* (NO_HZ_FULL, rcu_nocb).
  https://discourse.ubuntu.com/t/enable-low-latency-features-in-the-generic-ubuntu-kernel-for-24-04/42255
- **[Ref55]** Linux kernel docs, *KVM halt polling*.
  https://docs.kernel.org/virt/kvm/halt-polling.html ; KVM wiki
  *Virtio/Block/Latency*. https://www.linux-kvm.org/page/Virtio/Block/Latency
- **[Ref56]** Oracle, *Introduction to VirtIO Part 2 — vhost* (dataplane
  noyau, moins de context switches).
  https://blogs.oracle.com/linux/introduction-to-virtio-part-2-vhost
- **[Ref57]** Firecracker docs, *kernel-policy.md* (cmdline par défaut du
  kernel invité).
  https://github.com/firecracker-microvm/firecracker/blob/main/docs/kernel-policy.md
- **[Ref58]** Depot.dev, *How we got microVMs booting under a second*
  (cmdline `quiet`, ~200-300 ms vers systemd). https://depot.dev/blog/optimizing-microvm-boot-times
- **[Ref59]** Linux kernel docs, *kernel-parameters* (`quiet`,
  `initcall_blacklist`, clocksource/tsc).
  https://docs.kernel.org/admin-guide/kernel-parameters.html ; davidv.dev,
  *Minimizing Linux boot times*, 2024. https://blog.davidv.dev/posts/minimizing-linux-boot-times

---

*Document généré par veille web (WebSearch/WebFetch sur sources primaires
USENIX, ACM, IEEE, arXiv, docs officielles), lecture directe des PDF
SYSTOR'09 et Catalyzer ASPLOS'20, et rapprochement avec les mesures internes
Mycel du 2026-11-03. Aucun autre fichier du repo n'a été modifié.*
