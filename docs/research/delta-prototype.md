# Prototype : compression delta binaire (style rsync)

> Crate : [`crates/myc-delta`](../../crates/myc-delta) — « Rsync-style binary
> delta encoding for similar-but-rewritten files ». Prototype autonome ;
> seule modification hors crate : une ligne dans les `members` du workspace.

## Motivation

La déduplication par blocs ([`chunk-prototype.md`](chunk-prototype.md),
crate `myc-chunk`) partage l'**identique** : deux chunks CDC se dédupliquent
si leurs contenus coïncident byte à byte. Or une recompilation réécrit les
octets *partout* : mesuré sur nos vraies données, `busybox` alpine 3.20 vs
3.21 ne partage que **1,93 %** de ses octets au niveau bloc. L'étude
[`dedup-and-snapshot-survey.md`](dedup-and-snapshot-survey.md) recommande
pour ce cas la compression *delta* (rsync, xdelta, bsdiff) : encoder la
nouvelle version comme « références vers l'ancienne + littéraux », à tout
offset grâce à une fenêtre glissante. Ce prototype répond à la question par
la mesure, sur les mêmes blobs que l'étude chunk : **le delta capture-t-il
ce que les blocs ratent ?**

## Algorithme

Réimplémentation pure Rust (std uniquement, zéro `unsafe`, aucune dépendance
nouvelle — `blake3` et `thiserror` sont déjà des deps du workspace) du schéma
rsync (Tridgell, 1999) :

1. **Index du source (ancien)** : découpage en blocs **fixes de 4 Kio**
   ([`BLOCK_SIZE`]). Chaque bloc *plein* est indexé par un hash faible —
   **buzhash** (polynôme cyclique 32 bits : `h = rotl(h,1) ^ TABLE[octet]`,
   table de 256 addendes générée **à la compilation** par SplitMix64 avec
   graine fixe, même règle de stabilité que `myc-chunk`) — et confirmé par
   un hash fort **BLAKE3**. Un même hash faible peut indexer plusieurs
   blocs. Un **filtre de Bloom** de 2^16 bits retient les hashes faibles
   présents.
2. **Balayage de la cible (nouveau)** : une fenêtre de 4 096 octets (4096
   étant multiple de 64, la contribution de l'octet sortant est non tournée :
   retrait en O(1)) glisse **octet par octet**. Si le hash faible passe le
   Bloom puis la table, et que le BLAKE3 de la fenêtre égale celui d'un bloc
   candidat → `COPY(offset du bloc source, 4096)`, et le balayage reprend
   *après* le match ; sinon l'octet laissé derrière devient littéral.
3. **Décodage** : `apply(source, delta)` rejoue les instructions ; toute
   erreur de format (troncature, magic, version, tag, `COPY` hors bornes)
   est un `DeltaError` typé (thiserror).

La propriété clé est testée et tient toujours :
`apply(source, delta(source, target)) == target`. Le hash faible ne fait que
*localiser* ; BLAKE3 *décide* — un faux positif coûte un hash de plus, il ne
peut jamais corrompre la sortie.

**Complexité** : O(source + cible) attendu (par octet de cible : une
rotation, deux XOR de table, un test de bit ; la HashMap n'est consultée que
sur hit Bloom, BLAKE3 que sur collision 32 bits réelle), O(source) mémoire
pour l'index (~40 o/bloc). Le balayage est glouton comme rsync : un match
consommé masque les chevauchements — suffisant pour un prototype.

## Format sérialisé

Encodage maison minimal, little-endian, auto-délimité :

```text
magic "MYDLT" (5 o) | version u8 (=1) | instructions…
COPY (tag 1) : u64 offset source | u32 longueur
LIT  (tag 2) : u32 longueur | octets bruts
```

Les littéraux sont stockés **non compressés** : sans zstd/deflate (périmètre
« aucune dépendance »), le texte LIT se comprimerait bien — à faire au
niveau transport (le hub compresse déjà les flux), pas dans le format.

## API

```rust
pub const BLOCK_SIZE: usize = 4096;
pub fn delta(source: &[u8], target: &[u8]) -> Vec<u8>;
pub fn apply(source: &[u8], delta: &[u8]) -> Result<Vec<u8>, DeltaError>;
pub fn delta_stats(delta: &[u8]) -> Result<DeltaStats, DeltaError>; // octets/ops COPY vs LIT
```

Binaire d'analyse : `cargo run --release -p myc-delta --example
delta-report -- SOURCE CIBLE` — affiche tailles, ratio, % MATCH/LIT, le
round-trip vérifié, et **côte à côte** la couverture dédup-blocs (myc-chunk,
même métrique que `dedup-report`) : script de mesure
`/home/nboukadoum-ext/build/delta-measure.sh`.

Tests (`cargo test -p myc-delta`, 10 tests + 1 doc-test) : round-trips
seedés (SplitMix64) sur vide/tiny, fichiers identiques (COPY intégral,
delta ~2 %), insertions, suppressions, réécritures aléatoires (40 tours),
**décalage d'un octet en tête** (la fenêtre glissante recolle partout :
97 % de COPY là où des bornes fixes perdraient tout), 1 MiB édité, données
sans rapport (tout LIT, surcoût exact 11 o), rejet des deltas malformées
(tronquées à toute position, magic, version, tag, COPY hors bornes), et
équivalence roll/recompute du buzhash.

## Résultats mesurés

Mêmes blobs que l'étude chunk (extraits du store `~/.mycel` via les
manifests alpine 3.20/3.21) + paire texte 100 Ko/10 lignes changées.
« Blocs » = % de la cible couvert par des chunks CDC du source ;
« delta COPY » = % de la cible référencé par un `COPY` ;
« taille delta » = % de la cible à transmettre.

| Paire (A = 3.20, B = 3.21) | Tailles (o) | Blocs : couverte | Delta : COPY | Delta : taille |
|---|---|---|---|---|
| `/bin/busybox` | 808 712 / 808 712 | 15 606 (**1,93 %**) | 24 576 (**3,04 %**) | 784 245 (**96,97 %**) |
| `/lib/ld-musl-x86_64.so.1` | 649 832 / 662 120 | 218 574 (**33,01 %**) | 233 472 (**35,26 %**) | 429 415 (**64,85 %**) |
| `libcrypto.so.3` | 4 482 384 / 4 498 768 | 617 035 (**13,72 %**) | 716 800 (**15,93 %**) | 3 784 459 (**84,12 %**) |
| `text-base` vs `text-edited` (10 lignes) | 102 403 / 102 351 | 100 270 (**97,97 %**) | 98 304 (**96,05 %**) | 4 375 (**4,27 %**) |

Round-trip `apply == cible` vérifié sur les 4 paires. Lecture honnête :

- **Texte à éditions locales** : delta quasi nul (**4,27 %** de la cible),
  comme attendu ; la CDC fait mieux en *couverture* (97,97 % vs 96,05 %) car
  elle n'a ni queue < 4 Kio ni fenêtre plein-bloc à resynchroniser — mais en
  *transmission*, les deux approches envoient quelques % du fichier.
- **Binaires recompilés** (ld-musl, libcrypto) : le delta capte
  systématiquement **un peu plus** que les blocs (+2,3 pts musl, +2,2 pts
  libcrypto) — la fenêtre glissante rattrape, à tout offset, ce que les
  bornes CDC ratent — mais l'essentiel reste littéral (65 % et 84 %).
- **busybox, le cas « 2 % »** : le delta ne le « débloque » pas. Avec une
  granularité d'octet (fenêtre à tout offset), seuls **3,04 %** réapparaissent
  : la recompilation 3.20 → 3.21 réécrit réellement les octets partout, ce
  n'était **pas** un artefact d'alignement des bornes 4 Kio. Réponse à la
  question motivation : le delta complète utilement les blocs (musl,
  libcrypto, textes décalés), il ne remplace pas l'envoi du fichier sur les
  recompilations totales.

## Limites

- **Blocs 4 Kio fixes côté source** (choix de protocole, cf. rsync) : un
  match exige 4 096 octets *consécutifs* identiques ; des éditions dispersées
  (recompilation) court-circuitent les matches là où bsdiff — matching à
  granularité quelconque par table de suffixes — ferait mieux. À l'inverse,
  et contrairement aux bornes fixes, **les insertions/suppressions ne
  décalent rien** côté cible : la fenêtre glisse à tout offset (testé).
- **Queue du source** (< 4 Kio) non indexée : jamais matchée, ~2 Ko de LIT
  en fin de cible (visible sur le cas texte : 4 047 o LIT dont la queue).
- **Glouton** : pas de recherche d'alignement optimal (comme rsync).
- **Littéraux non compressés** (périmètre sans dépendance) : voir format.
- **Delta liée au couple (base, cible)** : il faut *choisir* la base, là où
  la dédup bloc est automatique contre tout le store.
- Perf mesurée : ~instantané en release sur 4,5 Mo (Bloom → HashMap → BLAKE3
  par niveaux de rareté croissante).

## Usage futur (hub)

- **Sync incrémentale** : pour un fichier mis à jour, le hub choisit une
  base déjà stockée, le client envoie `delta(base, nouveau)` ; le hub
  reconstruit par `apply` et pose les chunks manquants. Le delta ne remplace
  pas la dédup bloc, il la complète : envoyer `delta` *plus* les chunks
  CDC manquants.
- **Choix de la base par fichier** — candidate fonction future du hub :
  parmi les fichiers stockés, maximiser les chunks CDC partagés avec la
  cible (l'index de hashes existe déjà côté chunk-store), départage par
  taille ; la table de mesures ci-dessus est le guide de décision par type
  de fichier : éditions locales/décalages → delta ; binaires recompilés →
  envoyer le fichier entier.
