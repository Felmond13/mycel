# Images Docker vs environnements Mycel — comparatif mesuré

Cinq images réelles de Docker Hub, mesurées des deux côtés sur la même
machine (i5-1145G7, WSL2, Docker 29.1.3, myc 0.2.0), le même jour, mêmes
commandes. Brut archivé dans ce répertoire (`raw.txt`, `run-*.json`) —
jamais modifié après publication.

## Résultats

**Démarrage** (même commande des deux côtés, moyenne de 10 runs hyperfine ;
mémoire = pic résident de l'arbre de processus) :

| Image | docker run | myc run | mémoire docker | mémoire myc |
|---|---|---|---|---|
| alpine:3.20 `echo hi` | 660 ms | **28 ms** (24×) | 29,1 Mo | **5,7 Mo** |
| alpine:3.21 `echo hi` | 640 ms | **25 ms** (26×) | 29,2 Mo | **5,7 Mo** |
| nginx:1.27-alpine `nginx -v` | 667 ms | **112 ms** (6×) | 29,2 Mo | **6,2 Mo** |
| redis:7-alpine `redis-server --version` | 593 ms | **89 ms** (7×) | 28,9 Mo | ~6 Mo |
| python:3.12-alpine `python3 -V` | 681 ms | **137 ms** (5×) | 29,2 Mo | **6,4 Mo** |

**Acquisition** (médiane de 3 essais ; à froid = absent puis `docker rmi` /
`myc rm` + `gc` entre essais ; à chaud = déjà présent) :

| Image | docker à froid | myc à froid | docker à chaud | myc à chaud |
|---|---|---|---|---|
| alpine:3.20 | 3,19 s | **1,87 s** | 1,74 s | 1,89 s (+0 o) |
| alpine:3.21 | 3,24 s | **1,70 s** | 1,65 s | 1,83 s (+0 o) |
| nginx:1.27-alpine | 4,49 s | **3,83 s** | 1,95 s | 3,72 s (+0 o) |
| redis:7-alpine | 3,21 s | 3,32 s | 1,73 s | 3,05 s (+0 o) |
| python:3.12-alpine | 3,67 s | **2,90 s** | 1,61 s | 2,47 s (+0 o) |

**Disque** (5 images) : somme des `.Size` Docker = 59,7 Mo (le daemon
containerd affiche la taille **compressée** des couches) ; store Mycel =
**135 Mo physiques** (fichiers **non compressés**, clés du hardlink
zéro-copie à l'exécution). Lecture honnête : Mycel paie ~2× en disque pour
gagner 5-26× au démarrage et la dédup inter-images — ré-ingérer
`alpine:3.21` quand `3.20` est présent ajoute **80 Ko** (3 blobs uniques,
mesuré) ; docker re-télécharge la couche entière à chaque version.

**Diff exact entre versions** (impossible côté Docker) :
`myc diff alpine:3.20 alpine:3.21` → 47 fichiers : 10 ajoutés, 7 retirés,
30 modifiés — 7,4 Mo de contenu réellement nouveau, 480 inchangés.

## Points d'honnêteté

- `redis:7-alpine` a d'abord **échoué** côté myc (`chown: Invalid argument`
  dans l'entrypoint) : le mapping subuid exige le paquet `uidmap` (même
  mécanisme que Podman rootless) — installé, redis passe ensuite. Limitation
  documentée du repli single-uid.
- Les deltas « à froid » des répétitions myc montrent un gc à période de
  grâce (les blobs récents ne sont pas immédiatement libérés) ; la dédup à
  l'ingest elle-même est vérifiée séparément (+80 Ko, cf. ci-dessus).
- Docker tient ses chiffres à chaud (1,6-1,9 s) via son daemon ; myc n'a
  ni daemon ni privilège — et chaque mesure `myc` part de zéro processus.

## Reproduire

```bash
cd bench/image-compare && bash run.sh
```
