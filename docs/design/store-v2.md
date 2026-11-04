# Store v2 — Déduplication par blocs (transparente, rétro-compatible)

> Design implémenté dans `crates/myc-store/src/chunks.rs`. État : opt-in
> via `MYCEL_CHUNKED=1`, mesuré sur alpine 3.20 → 3.21 le 2026-11-03.
> Contexte amont : [`docs/research/chunk-prototype.md`](../research/chunk-prototype.md)
> (prototype CDC) et [`docs/research/dedup-and-snapshot-survey.md`](../research/dedup-and-snapshot-survey.md)
> (veille : FastCDC/Gear, packs façon Data Domain, filtres de Bloom).

## 1. Objectif et contrainte dur

Le store v1 déduplique au **fichier entier** : deux fichiers partagent du
stockage ssi leur BLAKE3 est identique. La v2 ajoute la déduplication **par
blocs** (chunks content-defined) sans rien casser :

- **zéro changement d'id** : manifestes et blobs gardent exactement le même
  format, le même nommage et les mêmes ids (`myc1-…`, BLAKE3 du JSON
  canonique). Un store v2 est lu par tout lecteur v1.
- **zéro rupture du chemin rapide** : les hardlinks zéro-copie de `myc-run`
  matérialisent les rootfs en linkant des **fichiers** du store. La v2
  continue donc d'écrire le fichier blob v1 **intégralement** (matérialisation
  inchangée) ; les chunks servent à la **déduplication de transfert** (le hub
  n'enverra que les chunks manquants).
- **opt-in** : sans `MYCEL_CHUNKED=1`, le comportement binaire, le layout
  disque et les performances d'aujourd'hui sont strictement inchangés.

## 2. Architecture

```text
                       ingestion (put_blob_with_mode)
                                  |
                 v1 (toujours) :  |  v2 (opt-in MYCEL_CHUNKED=1)
                                  |
              objects/ab/<hash>   |   blob ≥ 64 KiB et ≤ 256 MiB ?
              (fichier entier,    |          |
               inchangé)          |          v
              hardlink myc-run <--+   myc-chunk : FastCDC/Gear
                                       (2–16 KiB, moyenne 4 KiB)
                                              |
                              chunks manquants uniquement (Bloom + index)
                                              |
                                              v
     .mycel/packs/pack-<blake3-du-pack>.zst    chunks concaténés,
                                              1 frame zstd indépendante
                                              par chunk (niveau 3)
     .mycel/packs/pack-<…>.idx    hash chunk -> (offset, taille comp, taille)
     .mycel/packs/pack-<…>.bloom  filtre de Bloom des chunks du pack
     .mycel/chunks/<hash-blob>.chunks   sidecar : liste ordonnée des chunks
                                        du blob + table des packs utilisés
```

- **Pack** : immuable, nommé par le BLAKE3 de ses octets → le même *ensemble
  de chunks manquants* produit toujours le même nom (ingest idempotent, pas
  de collision nommable). Compression **par chunk** (frames zstd
  indépendantes, comme restic) : relire un chunk ne décompresse pas le pack
  entier.
- **Bloom** (cible 0,5 % de faux positifs, double-hash
  Kirsch–Mitzenmacher sur les 16 premiers octets du BLAKE3 du chunk) :
  répond « ce chunk est-il dans ce pack ? » sans toucher l'index disque —
  le *summary vector* de Data Domain. Un faux positif ne coûte qu'un
  chargement d'index : la réponse finale (`has_chunk`) n'a **jamais** de
  faux positifs (l'index `.idx` confirme).
- **Sidecar** (`chunks/<blob-hash>.chunks`) : la *recette* du blob — chunks
  dans l'ordre + table des packs. C'est la seule vérité par blob ; le
  réassemblage (`reassemble_blob`) reconstruit l'octet d'origine et
  revérifie le BLAKE3 du blob (garantie de round-trip, testée).
- **ChunkId = [u8; 32]** (BLAKE3 du chunk), API publique du store :
  `has_chunk`, `missing_chunks(&[ChunkId])`, `blob_chunks(hash)`,
  `reassemble_blob(hash)`, `chunk_stats()`.

### Écriture (`put_blob_with_mode`)

1. Le flux est écrit comme aujourd'hui (tmp + `persist_noclobber`) : v1
   d'abord, quel que soit le mode.
2. Si chunking actif et taille ∈ [64 KiB, 256 MiB] et pas encore de sidecar :
   relire le fichier, découper (myc-chunk), calculer les BLAKE3 par chunk,
   interroger l'index (Bloom d'abord) ; **seuls les chunks manquants** sont
   compressés dans un nouveau pack (dédup intra-blob incluse : un chunk
   répété dans le fichier n'est stocké qu'une fois) ; le sidecar est écrit
   en dernier (`noclobber` : premier gagnant, ingest concurrent sûr).
3. Sur *dedup hit* v1 (blob déjà présent) : le sidecar est **backfillé**
   depuis le flux qu'on vient de lire — un store v1 existant se met à niveau
   progressivement, sans migration.

### GC

`gc()` reste l'autorité v1 (blobs référencés par les manifestes vivants).
Après la passe v1, la v2 balaie : sidecars des blobs morts supprimés, puis
tout pack qu'aucun sidecar survivant ne référence est supprimé (le pack
créé par le blob A mais partagé par le blob B survit à la mort de A). Le
comptage de références *par chunk* reste une étape future (limites §6).

## 3. Décision centrale : dédup TRANSPARENTE au manifeste

Le manifeste v1 est **strictement canonique et hasher** (`id = myc1-‖BLAKE3(canonical JSON)`).
Y ajouter un champ par fichier (liste de chunks) casserait **tous les ids
existants** — refs, pins, hub, manifests partagés hors-ligne. La décision
d'architecture v2 :

> Les chunks sont une couche du **store**, décrite par des *sidecars*
> dérivés (et re-dérivables) du contenu des blobs. Le manifeste ne connaît
> que le hash de fichier. Zéro changement d'id, zéro rupture.

Conséquences :

- tout outil v1 (hub actuel, `myc run`, export) fonctionne sans recompilation
  conceptuelle — la négociation par chunks sera additive côté protocole ;
- la recette de chunks d'un blob est reconstituable à tout moment
  (backfill), donc perdre des sidecars n'est jamais une perte de données ;
- le surcoût disque v2 est un **cache de transfert**, pas une source de
  vérité : supprimable à volonté (c'est ce que fait le GC).

## 4. Chiffres mesurés (alpine 3.20 → 3.21, amd64, 2026-11-03)

Protocole : deux stores vierges sur le même disque ; store v2 avec
`MYCEL_CHUNKED=1`, ingestion de `docker.io/library/alpine:3.20` puis
`:3.21` (script `/home/nboukadoum-ext/build/measure-v2.sh`, mesures via
`cargo run -p myc-store --example store-v2-report`, décomposition par
taille via `/home/nboukadoum-ext/build/decomp-v2.py`). « Nouveau » = octets
apparus dans le store entre l'ingest 3.20 et l'ingest 3.21.

| Métrique (2e ingest, 3.21 après 3.20) | v1 (fichiers entiers) | v2 (chunks) |
|---|---|---|
| Nouveaux blobs < 64 KiB (non chunkés, transférés entiers) | 247 931 o (18 fichiers) | 247 931 o |
| Nouveaux blobs ≥ 64 KiB | 7 468 327 o (10 fichiers) | **6 473 453 o** (1 480 chunks manquants) |
| **Transfert total** | **7 716 258 o** | **6 721 384 o (−12,9 %)** |
| Économie sur les fichiers chunkés (≥ 64 KiB) | — | **−13,3 %** |

Lecture :

- Sur les seuls fichiers chunkés (≥ 64 KiB), le second ingest ne stocke que
  86,7 % des octets : 994 874 octets de chunks de 3.21 existaient déjà dans
  les packs de 3.20.
- Ce gain « modéré » est **cohérent avec le prototype** : les gros fichiers
  d'alpine sont des binaires *reconstruits* (busybox ~2 % partagé, musl
  ~33 %, libcrypto ~14 % — mesures du prototype) ; la CDC ne fait pas de
  miracle sur des builds non reproductibles. Le gain attendu est massif sur
  les contenus versionnés/texte (98 % mesurés au prototype) et entre images
  dérivées d'une même base.
- Les 18 petits fichiers nouveaux (< 64 KiB, 247 931 o) sont identiques en
  v1/v2 : décision assumée (pas de chunking sous 64 KiB, la dédup
  fichier-entier les couvre quand ils se répètent).
- **Surcoût disque v2** (v1 blobs + v2 packs coexistent) : après 3.21,
  store v2 = 23 057 272 o (blobs 15 503 302 + packs 7 435 470 + sidecars
  118 500) vs store v1 = 15 503 302 o, soit **+48,7 %**. Détail : les packs
  compriment 13 950 527 o logiques de chunks en 7 435 470 o sur disque
  (×0,53) ; les sidecars ne pèsent que 118 500 o (20 blobs, ~34 o/chunk).
  C'est le prix du design v2 minimal : les blobs v1 restent obligatoires
  (matérialisation hardlink), les packs sont un *cache de transfert*
  compressé — supprimable (GC) et re-dérivable (backfill).
- Coût CPU (build debug, pour mémoire) : ingest 3.20 chunké 5,0 s / 3,3 s
  user vs 3,1 s / 1,3 s non chunké — chunking + zstd ≈ +2 s pour 7,4 Mio.
- Profil observé : 1 705 chunks uniques pour 7 477 074 o chunkés de 3.20,
  soit 4 386 o/chunk en moyenne (profil FastCDC 2–16 KiB, cible 4 KiB ✓).

*(Le script cité régénère le tableau ; les nombres sont ceux de la passe du
2026-11-03, manifests `myc1-d5f20fd…` (3.20) et consorts.)*

## 5. Tests (tout le workspace reste vert)

`cargo test -p myc-store` couvre la v2 :

- `chunk_roundtrip_reassembles_original` — écrire, relire, réassembler =
  original (BLAKE3 vérifié) ;
- `bloom_has_no_false_negatives_and_bounded_fp` — 10 000 chunks, zéro faux
  négatif, taux de faux positifs mesuré borné (< 4× la cible 0,5 %) ;
- `second_similar_blob_needs_few_new_chunks` — une édition de 64 octets au
  milieu d'un blob de 256 KiB invalide ≤ 8 chunks ; `missing_chunks`
  retourne exactement les chunks à transférer (< 25 % du blob) puis plus
  rien après ingest ;
- `repetitive_blob_dedups_within_itself` — 256 KiB constants → chunks
  uniques ≪ nombre de chunks ;
- `reingest_is_idempotent` — second put = zéro nouvel octet ;
- `dedup_hit_backfills_chunk_sidecar_for_v1_blob` — mise à niveau d'un
  store v1 ;
- `gc_keeps_packs_referenced_by_live_sidecars` — un pack partagé survit à
  la mort de son créateur, disparaît avec sa dernière référence ;
- `non_chunked_store_writes_no_chunk_files` / `small_blob_is_not_chunked`
  — le chemin par défaut n'écrit rien de v2.

## 6. Limites (assumées)

- **Le hub négocie aujourd'hui par blobs** : tant que le protocole
  (`myc-hub`) n'échange pas `missing_chunks`, le gain v2 n'est pas encore
  réalisé sur le réseau — cette v2 pose les fondations (API + formats +
  mesures), la négociation par chunks est l'étape suivante.
- **Pas de refcount par chunk** : le GC supprime des packs entiers (exact
  pour la granularité pack, jamais sous-optimal en dessous). Un chunk
  partagé par deux packs vivants existe deux fois — cas rare (un chunk
  manquant au moment de l'ingest n'aurait pas dû exister ailleurs).
- **Mémoire** : l'indexation lit le blob en RAM (garde-fou : blobs ≤ 256
  MiB chunkés) ; `has_chunk` scanne les Blooms (quelques KiB par pack) et
  ne charge les index que sur admission.
- **Dernier chunk court** (< 2 KiB) : stocké tel quel dans le pack (pas de
  fusion avec le précédent) — le pack absorbe le coût.
- **Faux positifs Bloom** : coût = un chargement d'index, jamais une erreur.

## 7. Plan d'activation par défaut

1. **Aujourd'hui** : opt-in `MYCEL_CHUNKED=1` (early adopters, CI interne).
2. **Hub par chunks** : le serveur expose `has_chunk`/`missing_chunks` ;
   le client pousse/tire par pack (delta de chunks) — le gain réseau mesuré
   en §4 devient réel ; validation bit-à-bit via `reassemble_blob`.
3. **Bascule par défaut** : `Store::open_chunked` devient le défaut du CLI
   une fois le hub par chunks déployé et le surcoût disque (+48,7 % mesuré
   sur alpine ; ~50 % des blobs en packs *compressés*) jugé acceptable au
   regard du gain de transfert ; `MYCEL_CHUNKED=0` reste une échappatoire.
4. **Après adoption** : refcount par chunk, fusion du chunk final court,
   dictionnaire zstd par pack (`zstd --train`) pour compresser les petits
   chunks comme des gros.
