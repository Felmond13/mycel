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
  *(Réalisé depuis — voir §8.)*
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
   *(Fait, au grain chunk — voir §8.)*
3. **Bascule par défaut** : `Store::open_chunked` devient le défaut du CLI
   une fois le hub par chunks déployé et le surcoût disque (+48,7 % mesuré
   sur alpine ; ~50 % des blobs en packs *compressés*) jugé acceptable au
   regard du gain de transfert ; `MYCEL_CHUNKED=0` reste une échappatoire.
4. **Après adoption** : refcount par chunk, fusion du chunk final court,
   dictionnaire zstd par pack (`zstd --train`) pour compresser les petits
   chunks comme des gros.

## 8. Hub par chunks (négociation push/pull au niveau bloc)

> Implémenté dans `crates/myc-hub` (`server.rs`, `client.rs`, `transfer.rs`,
> `chunkio.rs`, tests `tests/chunks.rs`). État : mesuré le 2026-11-03.

Le protocole `/api/v1` s'étend de trois routes **additives** — les routes
blobs v1 existantes restent inchangées et un client v1 fonctionne sans
aucune modification contre un hub v2 (et réciproquement) :

| route | verb | rôle |
|---|---|---|
| `POST /chunks/missing` | corps `{blob_hash, chunk_ids:[…64-hex…]}` | le serveur répond `{missing:[…], pack_hints:[…]}` : les chunks qu'il N'A PAS (Bloom d'abord, index `.idx` pour le verdict final — jamais de faux « présent »), et les packs qui contiennent les chunks demandés déjà présents (indices pour un futur transfert par pack) |
| `GET /chunks/{id}` | octets bruts du chunk (décompressés) | le serveur relit la frame zstd du pack, décompresse, **re-hache** contre l'id avant de répondre |
| `POST /chunks/{id}` | corps = octets bruts | réception d'un chunk : re-haché serveur contre l'id, rejet si différent (même règle que les blobs), mis en *staging* (`<store>/hub-staging/<id>`, adressé par contenu, renommage atomique) |
| `GET /blobs/{hash}/chunks` | `{chunk_ids:[…]}` | la recette ordonnée du blob selon le sidecar du serveur (404 sinon → le client retombe sur la route blob v1) |
| `POST /blobs/{hash}/chunks` | corps `{chunk_ids:[…]}` | assemblage : le serveur reconcatène staging + chunks de SES packs, **re-hache le blob entier** contre `hash`, puis `put_blob_with_mode` |

### Négociation et bascule automatique

Le `/ping` du hub porte désormais `"chunks": true|false` (le store du hub
est-il ouvert avec la couche v2 — `MYCEL_CHUNKED=1` côté `myc hub serve`).
`transfer::push`/`pull` (appelés tels quels par le CLI et le web) :

1. store local v2 (`store.chunked()`) **et** hub `chunks:true` → protocole
   chunk ;
2. sinon → chemin blobs v1 inchangé, silencieusement ; avec un message
   informatif quand seul le hub fait barrage (« hub … does not support
   chunk negotiation »).

`PushReport`/`PullReport` gagnent `chunked` et `chunks_uploaded/downloaded`
; les champs existants gardent leur sémantique (les octets comptés sont
les charges utiles : blobs entiers, chunks, recettes — les négociations
`/missing` et `/chunks/missing` ne sont pas comptées, par symétrie v1/v2).

### PUSH chunké (par blob manquant)

```text
client                                 hub (store chunké)
  │ POST /missing {hashes}          →  blobs manquants
  │ pour chaque blob manquant :
  │   sidecar local → chunk_ids
  │   POST /chunks/missing          →  missing (Bloom d'abord)
  │   POST /chunks/<id> ×n          →  staging (re-hachés)
  │   POST /blobs/<hash>/chunks     →  assemblage + put_blob
  │ PUT manifest, PUT ref           (une fois tous les blobs complets)
```

Blobs sans sidecar local (< 64 Kio, ou ingérés avant v2) : route blob v1
pour ce blob. Le serveur reconstruit **le blob complet** : staging + ses
propres packs relus, blob re-haché, `put_blob_with_mode` écrit le fichier
v1 (les pulls v1 continuent de marcher) **et** ré-indexe les chunks dans
SES packs — seule la partie réellement nouvelle est recompressée (les
chunks partagés dédupliquent via l'index existant).

### PULL chunké (symétrique)

```text
client (store chunké)                 hub
  │ GET manifest + ref
  │ blobs manquants locaux (Store::missing_blobs)
  │ pour chaque blob manquant :
  │   GET /blobs/<hash>/chunks    →  recette serveur (404 → blob v1)
  │   Store::missing_chunks(...)  →  chunks réellement absents locaux
  │   GET /chunks/<id> ×n         →  octets re-hachés contre l'id
  │   assemblage local (packs locaux + téléchargés),
  │   BLAKE3 du blob vérifié, put_blob_with_mode
```

Le pull chunké traite les blobs séquentiellement (l'assemblage tient le
blob en RAM, comme l'indexeur du store — garde-fou ≤ 256 Mio) ; le push
chunké garde le pool de 8 threads (lecture par chunk, ~16 Kio par tampon).

### Décisions

- **`GET /chunks/{id}` décompresse côté serveur** (choisi) plutôt que de
  servir une fenêtre brute `?pack=<h>&offset&len` (écarté) : un chunk fait
  ≤ 16 Kio, le coût est négligeable ; le client ne peut pas être induit en
  erreur par des offsets périmés après un GC qui aurait supprimé le pack ;
  et le serveur re-hache avant de répondre. `pack_hints` reste dans la
  réponse pour un futur transfert par pack entier.
- **Staging serveur adressé par contenu** (`hub-staging/`) : re-pousser un
  chunk déjà stagé est un no-op ; `chunks/missing` considère un chunk
  stagé comme présent (un push interrompu puis rejoué ne renvoie rien deux
  fois) ; l'assemblage consomme et supprime les fichiers stagés. Un staging
  orphelin (push abandonné) n'est jamais corrompant — il est re-vérifié à
  la lecture et supprimé s'il ne hache pas — mais il persiste sur disque
  (le GC v1 ne le connaît pas) ; son nettoyage restera à la charge d'un
  futur `hub gc`.
- **Le serveur reste l'autorité** : aucun hash client n'est cru — chunks
  re-hachés à la réception, blob re-haché à l'assemblage, recette plafonnée
  à la taille max de blob chunké (256 Mio) contre l'amplification.
- **Compat v1 par construction** : un hub chunké sert les blobs v1 (le
  fichier entier existe toujours) ; un hub v1 voit simplement des clients
  qui retombent sur `/blobs`. Testé dans les deux sens.

### Chiffres mesurés (`myc-hub` tests d'intégration, 2026-11-03)

Scénario « alpine de labo » : E1 = 2 gros fichiers structurés (~224 Kio,
aléatoire + région répétée + queue quasi-identique) + 1 Kio de config ;
E2 = mêmes fichiers avec 64 octets édités + 8 Kio appendés, config
inchangée, + un nouveau fichier de 2 Kio. *Push* : hub pré-chargé E1 reçoit
E2 depuis le store A. *Pull* : store B (déjà détenteur d'E1) tire E2.
Contrôles : même scénario par le chemin blobs v1 (mêmes contenus, mêmes
états initiaux), octets comptés côté client dans les deux modes.

| Mesure (E2 après E1) | v1 (blobs) | v2 (chunks) | économie |
|---|---|---|---|
| **Push** (hub a déjà E1) | 468 992 o (3 blobs) | **27 135 o** (3 chunks + 2 recettes) | **−94,2 %** |
| **Pull** (B a déjà E1) | 468 992 o (3 blobs) | **19 065 o** (3 chunks) | **−95,9 %** |
| Re-push / re-pull E2 | — | 0 o (0 chunk) | idempotent |

Lecture : 64 octets édités dans un blob de ~230 Kio n'invalident que 1–2
chunks (CDC déterministe, re-synchronisation ≤ 16 Kio) ; le fichier de
2 Kio passe entier dans les deux modes (sous le seuil de 64 Kio, dédup
fichier-entier). **Attention** : ce scénario isole le cas « contenu
versionné » (le meilleur cas, ~98 % au prototype). Sur le vrai corpus
alpine 3.20 → 3.21 de §4 (binaires *reconstruits*), le gain attendu est
celui mesuré au niveau store : ~6,7 Mio transférés au lieu de 7,7 Mio
(**−12,9 %**) — la négociation par chunks transfère exactement les
« nouveaux chunks » du tableau §4, plus la recette (~34 o/chunk).

- **Coût serveur** (build debug, localhost, pour mémoire) : push E2 chunké
  27 ms vs 11 ms en mode blob ; pull 79 ms vs 56 ms. Le surcoût =
  assemblage (relire/décompresser les chunks partagés + re-hacher le blob
  + re-découper/re-compresser les chunks nouveaux via `put_blob`) et la
  granularité HTTP par chunk. À l'échelle alpine (7,4 Mio), extrapolé des
  mesures §4 (~+2 s/ingest en debug), l'assemblage ajoute ~une passe
  d'ingest CPU par blob — le prix d'un stockage serveur v1-compatible.
- Round-trip vérifié : après pull chunké, `missing_blobs` vide (prêt à
  `run`), chaque blob re-haché, `reassemble_blob` == octets d'origine, et
  le hub lui-même resert le blob entier à un client v1.

### Limites et suite

- **Octets comptés = charges utiles** (corps de requêtes/réponses blob +
  chunk + recette) ; les en-têtes HTTP et les négociations ne sont pas
  comptés — le mode chunk fait plus de requêtes (une par chunk), son
  surcoût d'en-têtes n'apparaît donc pas dans le chiffre ; négligeable à
  l'échelle (66 o d'en-têtes pour ≤ 16 Kio de données).
- **Pull chunké séquentiel** (RAM), push parallèle ; un transfert par pack
  entier (`pack_hints` déjà renvoyés) réduirait les allers-retours.
- **API myc-store à compléter** (contourné dans `myc-hub`, à hoister) :
  1. *lecture d'un chunk isolé* absente de l'API publique → `myc-hub/src/chunkio.rs`
     re-implémente le parseur du format `.idx` (magique `MYCPACKI1`,
     stable et documenté) + décompression de frame + re-hachage ;
  2. *ingest de chunks individuels / écriture de sidecar* absents → le
     serveur reconstruit le blob complet et repasse par `put_blob_with_mode`
     (re-découpe + re-compression des chunks nouveaux) ; une future
     `Store::put_chunk` éliminerait ce passage.
- Le CLI n'a pas été modifié : la bascule automatique vit dans
  `myc-hub/transfer.rs`, appelé tel quel par `myc push`/`myc pull` et le
  dashboard ; seuls les nouveaux champs des rapports (`chunked`, compteur
  de chunks) ne sont pas encore affichés par `myc-cli` (champ libre pour
  une future ligne « pushed … (N chunks) »).

---

## 9. Modes disque (`MYCEL_CHUNK_MODE`) : arbitrage du surcoût mesuré

> Implémenté dans `crates/myc-store` (`ChunkMode`, sidecars lean, cache de
> blobs `blob_cache`). Mesuré le 2026-11-04 (binaire release, WSL2) sur
> alpine 3.20 + alpine 3.21 + nginx 1.27-alpine — store vierge par mode ;
> script `/home/nboukadoum-ext/build/store-modes.sh`, résultats bruts dans
> `/home/nboukadoum-ext/build/store-modes-results.txt`.

### 9.1 Le problème

Le v2 « historique » écrit le blob v1 **et** les packs : +48,7 % de disque
(§4). Or les packs n'ont d'utilité *réseau* (§8) que comme cache de
transfert ; *savoir ce qu'on a*, la négociation s'en contente des
sidecars. D'où trois modes, choisis par `MYCEL_CHUNK_MODE` (défaut
`transfer` = comportement §4 inchangé) et tranchés par la mesure (§9.4),
pas par opinion.

| | **transfer** (défaut) | **lean** | **packs-only** |
|---|---|---|---|
| Écrit à l'ingest (≥ 64 Kio) | blob v1 + pack + sidecar plein | blob v1 + sidecar **lean** (ids seuls, 32 o/chunk, aucun pack) | pack + sidecar plein, **pas de blob v1** |
| Fichiers < 64 Kio | blob v1 | blob v1 | blob v1 (rien d'autre ne les porte) |
| Disque (mesuré §9.4) | +47,8 % | **+0,70 %** | **−46,6 %** |
| Pull chunké (§8) | prêt | prêt (négociation par sidecars) | prêt |
| Push chunké **source** (§8) | prêt (packs) | non (pas de pack où lire le chunk → repli blob v1) | prêt (packs) |
| Hardlink `myc-run` | direct | direct | via réassemblage à la 1re utilisation |
| Coût du 1er run | — | — | +67 ms mesuré (alpine) |

### 9.2 Mécanique commune

- **`store/chunk-mode`** : écrit au premier usage v2 (premier gagnant,
  comme tous les fichiers du store). Un changement de mode est **refusé**
  tant qu'un blob n'existe *qu'en packs* (sans fichier v1) — ces octets
  deviendraient invisibles ; le message explique comment régénérer (un
  `myc run` de chaque environnement réassemble et met en cache, ou
  re-ingest). Une fois tous les blobs matérialisés, le switch est accepté
  (packs-only → lean, lean → transfer, etc.).
- **Oracle de connaissance unifié** : `has_chunk` / `missing_chunks`
  scannent les **sidecars** (les deux formats), quel que soit le mode.
  Complet même pour les packs, par invariant : un pack n'est jamais écrit
  sans le sidecar qui le référence, et le GC ne garde un pack que
  référencé par un sidecar survivant. Les Blooms restent sur disque (et
  dans `reassemble`) pour le protocole hub et la localisation par pack.
- **Sidecar lean** (`MYCCHNKL1`) : liste d'ids, sans table de packs. En
  lean, si *tous* les chunks d'un nouveau blob sont déjà connus (un autre
  sidecar les liste — on détient donc un blob les contenant), **rien
  n'est écrit** ; sinon on enregistre la liste complète (~32 o/chunk).
- **packs-only** : `Store::blob()` réassemble à la demande (BLAKE3
  revérifié), écrit le blob v1 en cache, pose un marqueur
  `blob-cache/<hash>` (contenu = taille, mtime = dernier usage) puis
  évictie en LRU jusqu'à `MYCEL_BLOB_CACHE_MB` (défaut 512 Mio) — sans
  jamais toucher aux packs/chunks ; les blobs référencés par un manifeste
  vivant sont protégés (v1 simple : LRU des non-référencés d'abord).
  `myc-run::materialize` est **inchangé** : il appelle `Store::blob`, qui
  matérialise puis hardlinke. `Store::evict_blob_cache()` est public (un
  futur `myc gc --lean` s'y branchera) ; le GC purge les marqueurs
  orphelins.
- **GC par mode** : transfer = comportement §2 ; lean = les sidecars lean
  ne référencent aucun pack, donc les packs d'une époque transfer deviennent
  orphelins et sont recyclés (cohérent : lean = sans packs) ; packs-only =
  sidecars des blobs morts supprimés, packs des sidecars vivants
  **conservés** (un blob évincé du cache garde ses chunks — testé).

### 9.3 Limites assumées

- **lean → transfer** : le backfill ne réécrit pas les packs d'un blob
  déjà sidecaré (raccourci d'idempotence). Migrer un store lean vers
  transfer = supprimer les sidecars et re-ingester (future commande
  dédiée possible).
- **lean, push chunké source** : `myc-hub/chunkio` lit les chunks dans
  les packs ; un store lean n'en a pas — le push d'un blob chunké depuis
  un store lean repasse par la route blob v1 (correct, non optimal). Une
  future source « re-découper le blob v1 à la volée » comblerait le trou
  sans packs.
- **packs-only + v1** : un store packs-only ouvert *sans*
  `MYCEL_CHUNKED=1` ne voit pas les blobs chunkés (la voie v1 reste
  strictement inchangée) — ouvrir toujours un store packs-only en v2.
- **packs-only** : export et hub servent via réassemblage (cache) ; un
  disque qui perd un pack perd le blob (en transfer/lean, les blobs v1
  restent la redondance).

### 9.4 Mesures (store vierge par mode, corpus ×3 images, amd64, release)

**Disque après ingest, avant tout run** :

| Mode | Blobs v1 (o) | Packs+idx+bloom (o) | Sidecars (o) | **Total (o)** | Mio | vs v1 |
|---|---:|---:|---:|---:|---:|---:|
| v1 (référence) | 62 975 020 | 0 | 0 | 62 975 020 | 60,06 | — |
| transfer | 62 975 020 | 29 633 971 | 470 759 | **93 079 750** | 88,77 | **+47,8 %** |
| lean | 62 975 020 | 0 | 439 421 | **63 414 441** | 60,48 | **+0,70 %** |
| packs-only | 3 496 176 | 29 633 971 | 470 759 | **33 600 906** | 32,04 | **−46,6 %** |

- Les packs sont **bit-à-bit identiques** entre transfer et packs-only
  (nommés par BLAKE3 de leurs octets : déterminisme vérifié sur tout le
  corpus).
- packs-only : 59,5 Mio de blobs ≥ 64 Kio deviennent 29,6 Mio de packs
  (zstd ×0,50) + 0,45 Mio de sidecars ; les 3,5 Mio restants sont les
  fichiers < 64 Kio (toujours en blobs v1).
- Sidecars lean (439 421 o) < sidecars pleins (470 759 o) : 32 vs 34
  o/chunk, malgré les sidecars « rien de nouveau » omis.

**Temps** (release ; l'ingest inclut le pull réseau — variance, détail
par image dans les logs du script) :

| Mode | Ingest total 3 images (ms) | `run alpine -- echo hi`, médiane de 3 (ms) | 1er run (ms) | runs 2–3 (ms) |
|---|---:|---:|---:|---:|
| v1 | 6 710 | 37 | 37 | 36–37 |
| transfer | 11 013 | 30 | 34 | 29–30 |
| lean | 6 789 | 31 | 36 | 31 |
| packs-only | 7 496 | 33 | **104** | 32–33 |

- **Ingest** : lean ≈ v1 (+1,2 %) ; packs-only +11,7 % (chunking + zstd,
  mais pas de double écriture du blob) ; transfer +64 % (double écriture ;
  l'ingest 3.21 de transfer a mesuré 4,7 s contre 1,5–1,8 s ailleurs —
  pull + indexation, à re-mesurer le hub chunké en vue).
- **Matérialisation** : à chaud, tous les modes sont équivalents
  (29–37 ms). En packs-only, le **premier** run paie le réassemblage des
  10 blobs chunkés d'alpine (~15,5 Mio décompressés) : 104 ms vs 32 ms
  ensuite, soit **+67 ms une fois** — imperceptible. Cache après 3 runs :
  10 blobs, budget défaut 512 Mio.

### 9.5 Recommandation : `lean` par défaut

L'hypothèse à tester (« lean par défaut — quasi zéro surcoût, migration
future vers packs-only ») est **confirmée par la mesure** :

- **+0,70 % de disque** pour une connaissance chunk complète (négociation
  hub prête côté pull, §9.1), +1,2 % d'ingest, runs identiques, blobs v1
  et hardlinks intacts : l'option est quasi gratuite.
- **transfer** reste le bon mode pour les **hubs/serveurs** et les
  machines qui **poussent** par chunks (§8) : là, les packs sont un cache
  de transfert effectif, pas du ballast. C'est aussi le défaut actuel —
  ne rien changer avant décision explicite.
- **packs-only** est la réponse disque contraint : −46,6 % mesurés,
  coût unique de ~70 ms au premier run d'un env — au prix d'une
  dépendance forte aux packs (interop v1 cassée, réassemblage avant
  hardlink). Cible naturelle des clients une fois l'écosystème 100 % v2.
- **Trajectoire** : un store lean peut basculer en packs-only à tout
  moment (blobs présents ⇒ switch autorisé) ; l'inverse est gardé-fou
  (refus tant que des blobs ne vivent qu'en packs).

### 9.6 Tests

`cargo test -p myc-store` : **32 verts** (les 23 existants inchangés + 9
nouveaux). Nouveaux : noms de modes ; round-trip lean sans aucun pack ;
lean n'écrit rien quand tous les chunks sont connus ; round-trip
packs-only (pas de blob v1, matérialisation à la demande, cache) ;
éviction LRU ne touche jamais aux chunks (rebuild après éviction) ;
refus de changement de mode avec message de régénération puis acceptation
après matérialisation ; cache blob (record/LRU/orphelins).
`cargo fmt` + `cargo clippy -p myc-store --all-targets -- -D warnings`
verts.
