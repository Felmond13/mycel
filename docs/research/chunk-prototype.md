# Prototype : déduplication par blocs (content-defined chunking)

> Crate : [`crates/myc-chunk`](../../crates/myc-chunk) — « Content-defined
> chunking for block-level deduplication ». Prototype autonome, aucune
> modification des autres crates, aucun commit de format.

## Motivation

Le store Mycel v1 déduplique au niveau **fichier entier** : deux fichiers
partagent du stockage si et seulement si leur hash BLAKE3 est identique. Deux
versions successives d'un même binaire (musl, openssl) ou d'un même fichier
texte (config, logs) n'ont en général **aucun** octet en commun à ce niveau,
alors que leur contenu est presque identique. Ce prototype démontre ce qu'un
découpage en blocs *définis par le contenu* (CDC) apporterait : dédupliquer
des blocs de lignes/fragments répétitifs **dans** et **entre** les fichiers.

## Design

Algorithme inspiré **FastCDC** (Xia et al., 2016) avec hash **Gear 64 bits** :

- **Hash Gear** : `h = (h << 1) + GEAR[octet]` — un lookup table + une
  addition par octet. La table `GEAR` (256 addendes pseudo-aléatoires) est
  générée **à la compilation** par SplitMix64 avec une graine fixe
  (`GEAR_SEED`) : les bornes de chunks sont déterministes et stables entre
  machines et releases. Chaque bit de sortie dépend des 64 octets précédents
  (fenêtre glissante implicite).
- **Découpe normalisée à deux phases** : une coupure est déclarée quand
  `h & MASQUE == 0`.
  - Phase 1, longueurs `[MIN, CENTRE)` : `MASK_S`, 11 bits positionnés →
    probabilité 2^-11 (run attendu 2 048 o).
  - Phase 2, longueurs `[CENTRE, MAX)` : `MASK_L`, 13 bits → probabilité
    2^-13 (run attendu 8 192 o).
  - `CENTRE = MIN + (MAX - MIN) / 2 = 9 216 o` ; au-delà de `MAX`, coupure
    forcée. Les bits des masques sont dispersés sur le u64 (tirage SplitMix64)
    et `MASK_S ⊂ MASK_L`.
- **Profil** : `MIN = 2 048`, `AVG = 4 096`, `MAX = 16 384` octets. La
  normalisation centre la moyenne mesurée à ~4,2–4,4 Ko (cf. mesures), contre
  ~5,9 Ko pour un masque unique équivalent.
- **Empreinte des blocs** : BLAKE3 par bloc (déjà dépendance du workspace).
  Zéro dépendance nouvelle, **aucun `unsafe`**.

Complexité **O(n)** en temps (un octet = un lookup + un add + un test de
masque, BLAKE3 linéaire), **O(1)** en espace par décision de coupure.

API publique :

```rust
pub const MIN_SIZE: usize = 2048;   // pub AVG_SIZE / MAX_SIZE aussi
pub fn chunk_bounds(data: &[u8]) -> Vec<(usize, usize)>; // (début, fin) qui pavent l'entrée
pub fn chunk_hashes(data: &[u8]) -> Vec<[u8; 32]>;       // BLAKE3 de chaque bloc
```

Binaire d'analyse : `cargo run -p myc-chunk --example dedup-report -- FICHIERS…`
affiche par paire les octets communs au niveau FICHIER (égalité binaire) vs au
niveau BLOC (chunks partagés) et le ratio. Std uniquement + myc-chunk.

Propriétés testées (`cargo test -p myc-chunk`, 10 tests + 1 doc-test) :
déterminisme ; bornes min/max respectées (données aléatoires **et**
pathologiques constantes) ; **unshift-stability** (1 octet inséré en tête →
tous les blocs sauf ~1–2 réapparaissent à l'identique) ; édition locale →
blocs locaux seuls invalidés ; tailles distribuées autour de la moyenne
(moyenne ±25 % de 4 096, <10 % de blocs au plafond MAX).

## Résultats mesurés

Blobs réels extraits du store `~/.mycel` via les manifests
`docker.io/library/alpine:3.20` et `:3.21` (objets BLAKE3 du store,
chemins retrouvés dans les manifests). « Niveau BLOC » = octets du fichier B
couverts par des chunks dont le hash existe aussi dans A.

| Paire (A = 3.20, B = 3.21) | Tailles (o) | Fichier : octets communs | Bloc : octets communs (part, blocs) |
|---|---|---|---|
| `/bin/busybox` | 808 712 / 808 712 | 0 (0 %) | 15 606 (**1,93 %**, 4/207) |
| `/lib/ld-musl-x86_64.so.1` | 649 832 / 662 120 | 0 (0 %) | 218 574 (**33,01 %**, 49/155) |
| `libcrypto.so.3` (`/lib` vs `/usr/lib`) | 4 482 384 / 4 498 768 | 0 (0 %) | 617 035 (**13,72 %**, 167/1030) |

Preuve du concept « blocs de lignes répétitifs » — fichier texte construit
(100 Ko, 1 597 lignes) vs version où **10 lignes au milieu changent** :

| Paire | Tailles (o) | Fichier | Bloc |
|---|---|---|---|
| `text-base.txt` vs `text-edited.txt` | 102 403 / 102 351 | 0 (0 %) | 100 270 (**97,97 %**, 26/27) |

Statistiques de chunking observées : moyennes par fichier 3 791–4 442 o,
minimum structurel 2 048 (dernier bloc du fichier plus court possible),
maximum 16 384.

Lecture :

- **Fichiers texte / données répétitives** : gain massif (~98 % partagés là
  où v1 stocke deux fois) — c'est le cas d'usage visé.
- **Bibliothèques voisines** (musl +33 %, libcrypto +14 %) : gain réel que
  v1 ne capture pas du tout (0 %).
- **busybox 3.20 vs 3.21** : presque rien (1,9 %) — deux reconstructions
  diffèrent partout à la granularité 4 Ko ; honnêtement mesuré, la CDC ne
  fait pas de miracle sur des binaires reconstruits.

Surcoût manifeste estimé (borne haute, ~40 o/chunk : 32 hash + 4 offset +
4 taille) : **~1 %** de la taille des fichiers (0,98–1,02 % mesuré sur les
échantillons ci-dessus).

## Limites et intégration future

- **Manifeste v2 requis** : le manifeste v1 référence un hash par fichier ;
  un store par blocs doit lister les chunks (hash + longueur) de chaque
  fichier, soit ~40 o/chunk (~1 % du volume, cf. ci-dessus) — à compacter
  (delta/varint) et à évaluer sur un store complet avant adoption. Prévoir
  le chiffrement/dédup croisé plus tard.
- **Granularité** : moyenne 4 Ko (plage 2–16 Ko). Des blocs plus petits
  augmenteraient le gain sur les binaires mais aussi le surcoût manifeste
  et le nombre de petits objets (fanout du store à repenser, p.ex. pack
  files façon git/casync).
- **Réplication/GC** : il faut un compteur de références par chunk et non
  par blob ; la suppression ne peut plus supprimer un fichier isolément.
- **Dernier bloc court** (< 2 048) : tel quel il crée des petits objets ;
  une future intégration pourrait le fusionner au bloc précédent ou dans un
  pack.
- **Compatibilité v1** : aucun changement — le prototype vit dans
  `myc-chunk`, aucune autre crate ne l'utilise, le format fichier-entier et
  les manifestes existants restent inchangés. Une intégration se ferait
  derrière un **format v2** opt-in de `myc-store`, décidé après mesure du
  surcoût réel sur un store complet.

## Reproduire

```bash
cargo test -p myc-chunk
cargo run -p myc-chunk --example dedup-report -- FICHIER_A FICHIER_B
# Mesures de ce document : scripts /home/nboukadoum-ext/build/chunk-measure.sh
# et chunk-libcrypto.sh (extraient les blobs des manifests alpine 3.20/3.21,
# génèrent la paire texte 100 Ko, exécutent dedup-report).
```
