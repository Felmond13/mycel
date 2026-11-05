# Warm pool : verdict mesuré — pas intégré au chemin produit (2026-11-05)

Le prototype `bench/warm-pool` avait mesuré un handoff à **28 ms médiane**
(VM pré-bootée jusqu'à « agent attend sur scratch », puis réécriture du
scratch + réveil) contre **299 ms** de boot à froid — ×10,7, promesse
tenue. Puis l'accélérateur snapshot Firecracker est arrivé **en produit**
(`crates/myc-backend/src/firecracker.rs` : restore d'une VM mise en pause,
~22 ms spawn→resumed mesurés ci-dessous). La question tranchée ici, par la
mesure : un pool de VM déjà bootées-at-agent-wait apporte-t-il encore assez
sur le chemin produit pour se justifier comme feature ?

**Réponse : non.** Le gain attribuable en propre au pool (ce que « VM déjà
restaurée + agent en attente » élimine du e2e) est de **22 ms sur ~229 ms
e2e (~10 %)** à machine calme — sous les deux seuils d'intégration
(< 40 % du e2e, < 50 ms absolus). Le reste de son plafond théorique
(~70-90 ms) est du coût **hôte** (réseau + teardown) que des correctifs
produit récupèrent pour tous les runs — deux sont livrés ici, mesurés.
Détail ci-dessous.

## Méthode

- Même mètre que `bench/snapshot-accel` : hyperfine + scripts qui
  **vérifient la sortie (`ok`) et le code de retour à chaque run** — un
  run silencieusement cassé échoue la mesure.
- Commande de référence : `myc run --engine kvm-microvm alpine:3.20 --
  /bin/echo ok`, **root** (donc pont + bail + tap + ARP + egress NAT sur
  le chemin), build **release**, snapshot chaud (restore).
- **Machine partagée** (WSL2, i5-1145G7 4C/8T, 11 GiB) : la charge des
  autres sessions oscille entre <1 et 22 durant la campagne. Chaque série
  enregistre donc la charge ; les comparaisons avant/après sont mesurées
  en **alternance stricte A,B,A,B…** pour que la dérive frappe les deux
  séries symétriquement.
- Décomposition par phase : `MYCEL_VM_TRACE=1` (PhaseTimer ajouté dans
  `firecracker.rs` pour cette campagne) — gates / net / fingerprint /
  spawn+API / load_snapshot / inject / attente-sortie-VM / teardown
  interne. Deux sondes décortiquent l'attente : instant du **marker de
  sortie MYCE** après resume (ramasse agent + exec + sync côté invité) vs
  mort du process VM après le marker (reboot invité + sortie FC).

## 1. La référence à battre — e2e snapshot chaud (root), avant correctif

| Fenêtre de charge | n | médiane | p95 | charge médiane |
|---|---|---|---|---|
| calme (A/B alterné) | 20 | **228,5 ms** | 257,0 ms | ~1,5 |
| modérée (hyperfine) | 30 | 298,5 ms | 418,5 ms | ~2-4 |
| chargée (A/B alterné) | 30 | 2015,5 ms | 4890 ms | ~5,7 |

Contexte : plancher CLI (`myc --version`) **2,3 ms** médiane ; variante
`--net none` (root, pas de bail/tap/egress) 190,5 ms médiane / 298 ms p95
(n=30, fenêtre modérée). Le chiffre publié avant cela (171,5 ms,
`docs/benchmarks/snapshot-accelerator.md`) était non-root sans réseau :
cohérent — le réseau hôte + son teardown coûtent ~30 + ~50 ms à machine
calme.

## 2. Décomposition — où partent les millisecondes (avant correctif)

Médianes par phase (n=20 tracés ; « calme » charge ~1, « chargée » charge
8-14) :

| Phase | calme | chargée | éliminable par un pool ? |
|---|---|---|---|
| gates (résolution assets + version FC) | 1 | 18,5 | non |
| **net** (pont + bail + tap + ARP + egress NAT) | 28 | 238 | seulement si un daemon pré-détient bail+tap |
| fingerprint + lock (sha mémoïsées) | 0 | 0 | non (nécessaire au match) |
| **spawn FC + attente API** | 11 | 27 | **oui (VM déjà vive)** |
| **load_snapshot (restore)** | 11 | 99,5 | **oui (VM déjà restaurée)** |
| inject (écriture scratch + resume) | 0 | 7,5 | non |
| **attente sortie VM** | 141 | 739 | **non** : l'agent du pool exécute la même boucle |
| ↳ sonde : marker MYCE après resume | **80** | 429,5 | non (plancher invité) |
| ↳ sonde : mort du process après le marker | 60 | 267,5 | non (invité) — mais **correctible côté hôte** (§4) |
| **teardown** (iptables + tap + bail + tailer + state dir) | 51 | 205 | seulement si un daemon l'absorbe en asynchrone |
| somme des médianes | 290 | ~1530 | |

## 3. Plafond de gain du pool — les trois lectures honnêtes

1. **En propre** (« VM déjà bootée-at-agent-wait » = spawn FC + API +
   restore) : **22 ms à machine calme, ~10 % du e2e** — sous les deux
   seuils (10 % < 40 % ; 22 ms < 50 ms). En fenêtre chargée, le restore
   souffre de la contention (127 ms « économisés »)… sur un e2e qui
   dépasse alors 1,5-3 s : le pool y laisse encore ~1 s (le ramasse
   agent + exec + propagation de sortie, 429 ms médian chargés, ne bouge
   pas). Il ne rend pas « latence-critique » une machine affamée.
2. **+ réseau pré-détenu** par un daemon (bail+tap+ARP créés au démarrage
   du pool, transférés au handoff) : +28 ms calme, +238 ms chargée.
3. **+ teardown absorbé** en asynchrone par le daemon (le runner rend le
   code dès le marker) : +40-51 ms calme, +205 ms chargée.

Plafond d'architecture complet : **~90-101 ms sur 228,5 ms (40-44 %) à
machine calme** ; ~570 ms sur ~1530 ms (37 %) en machine chargée. Mais ce
plafond n'est « pool » que par accident : les niveaux 2-3 sont des coûts
**hôte** (spawns `ip`/`iptables`/`sysctl` — ~15 process par run côté
setup, ~4 côté teardown) que des correctifs produit récupèrent pour tous
les runs (§4 en livre deux, mesurés). Après ces correctifs, le résiduel
spécifique au pool reste son niveau 1 : **~22 ms**. Et même un pool
**parfait** paierait le plancher invité : marker à **73-80 ms** après
resume + ~27 ms de reap — soit un e2e pool projeté ~105-115 ms, contre
**201,5 ms mesurés** pour le chemin produit corrigé (§4) : l'écart
restant (~90 ms) est exactement le réseau+teardown hôte, pas la VM.

Pourquoi le **28 ms du prototype ne se transpose pas** : il mesurait le
RTT d'une requête curl vers un serveur déjà vif — pas un cycle de vie de
conteneur. Un `myc run` doit propager code de sortie + sortie console puis
détruire la VM (contrat « une VM propre par run », canal scratch à usage
unique) : même servi par un pool, il paie le ramasse agent + exec +
propagation de sortie (~80 ms à calme). S'ajoutent les coûts structurels
d'un pool produit : remplissage = restore à froid **en tâche de fond**
(~1,9 s CPU) qui vole des cycles au run suivant sur une machine 4C/8T ;
N × 512 MiB résidents ; corrélation bail-IP ↔ empreinte snapshot (N baux
= N instantanés à créer) ; un nouveau processus long root à sécuriser et
exploiter (verrou par VM, TTL, purge à l'arrêt).

## 4. Livré à la place : sortie anticipée au marker (+ purge conditionnelle)

La décomposition désigne deux cibles rentables et sans daemon :

- **Attente du reboot invité** : l'agent écrit le marker de sortie MYCE +
  `sync` **avant** de rebooter ; le sentinel MYCR écrit avant le resume
  rend la lecture prématurée sûre. Le host scrutait le process FC toutes
  les 20 ms et attendait le reboot complet (sonde : +60 ms à calme,
  +267 ms chargés après le marker). → le host scrute maintenant le marker
  toutes les **5 ms** et **SIGKILL le VMM dès observation** (le code de
  sortie est durable ; le tailer console draine ce qui reste).
- **Purge iptables inutile** : un run sans port publié ne pousse aucune
  règle et son bail est déjà purgé au setup ; la purge du teardown
  (2 spawns `iptables -S`) devient conditionnelle.

**Mesure avant/après** (alternance stricte A,B,A,B — même fenêtre, même
charge médiane des deux côtés) :

| Fenêtre | | avant | après | gain |
|---|---|---|---|---|
| calme (~1,5) | n=20+20 | 228,5 / 257,0 ms (méd/p95) | **201,5 / 230,0 ms** | **−27 ms (−12 %)** |
| chargée (~5,7) | n=30+30 | 2015,5 / 4890 ms | **1525,5 / 3249 ms** | **−490 ms (−24 %)**, p95 **−1641 ms** |

Attribution par phase (n=20, calme) : attente sortie VM 141 → **104 ms**
(le marker invité à 73 ms devient le plancher), teardown 51 → **40 ms**
(purge 3 → 0) ; somme des médianes 290 → 232 ms. La correction est
d'autant plus rentable que la machine est chargée : le reboot invité
sous contention CPU était le poste qui explosait (p95 −1,6 s).

Non-régression : `bench/microvm/e2e-vm.sh --with-ports` **27/27 PASS**
(codes de sortie 7/42 propagés, redis `-p 6399:6379` +PONG, **zéro
résidu** après chaque run — VMM, taps, DNAT, ARP, state dirs) ; tests
workspace verts ; fmt/clippy `-D warnings` propres.

## 5. Ce qui reste sur la table (prochaines leviers, mesurés)

- **Teardown tap+bail** : 34-38 ms médians à calme (2 `ip neigh del` + 1
  `ip link del` séquentiels). Pistes : `ip -batch`, parallélisation des
  deux `neigh del`.
- **Setup réseau** : 27-28 ms calmes / 43-238 ms chargés (~15 spawns
  `ip`/`iptables`/`sysctl` largement indépendants → parallélisables).
- **Le pool reste le repli documenté** pour les plateformes **sans**
  restore vCPU (macOS HVF, libkrun sans API snapshot) : matrice dans
  `docs/isolation-engines.md`, kit `bench/warm-pool/`. Sur le chemin
  Linux/WSL2 avec l'accélérateur Firecracker, il est enterré comme
  feature produit — avec ces chiffres.

## Reproduire

```bash
cargo build --release -p myc-cli
sudo bash bench/warm-pool/baseline.sh     # référence e2e (n=30) + plancher CLI + --net none
sudo bash bench/warm-pool/decompose.sh    # médianes par phase (n=20 tracés)
sudo bash bench/warm-pool/ab-fix.sh       # avant/après correctif, alterné (n=30+30)
```

Résultats bruts : `bench/warm-pool/results/` (jamais édités après
publication). Environnement : WSL2 6.18.54.1-microsoft, i5-1145G7,
Firecracker v1.17.0, noyau invité 6.12.115 (vmfs, ELF), 1 vCPU / 512 MiB,
myc 0.3.1 release, branche `feat/warm-pool` (base `4793a7b`) ; binaires
avant/après construits depuis les commits correspondants.
