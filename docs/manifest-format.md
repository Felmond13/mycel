# The Mycel manifest format (schema 1)

A manifest is a canonical JSON document describing a complete runnable
environment. Its identity is `myc1-<BLAKE3 hex of the canonical encoding>`:
two manifests with the same id are byte-identical environments, guaranteed.

## Top level

| Field | Type | Description |
|---|---|---|
| `schema` | int | Always `1` |
| `name` | string | Human name, e.g. `docker.io/library/alpine:3.20` |
| `origin` | string? | Provenance, e.g. `oci://docker.io/library/alpine:3.20` |
| `os`, `arch` | string | Target platform (`linux`, `amd64`/`arm64`) |
| `created` | string | RFC 3339 timestamp |
| `config` | object | Runtime configuration (below) |
| `entries` | array | The file graph, sorted by path, parents first |

## `config`

`env` (list of `KEY=VALUE`), `entrypoint`, `cmd`, `workdir`, `user` — the
exact semantics of the corresponding OCI image config fields. The effective
command of a run is `entrypoint + (override | cmd)`.

## `entries[]`

| Field | Type | Description |
|---|---|---|
| `path` | string | Absolute normalized path (`/usr/bin/env`), never contains `..` |
| `kind` | enum | `file`, `dir`, `symlink`, `fifo`, `char_device`, `block_device` |
| `mode` | int | Unix permission bits (low 12 bits) |
| `uid`, `gid` | int | Ownership |
| `size` | int | Content size in bytes (files, omitted when 0) |
| `blake3` | string | Content hash — **required** for files, the store key |
| `target` | string | Symlink target — required for symlinks |
| `device` | [maj, min] | Device numbers — devices only |

Hardlinks are represented as two file entries sharing the same `blake3`;
the store keeps one blob, materialization produces two hardlinks.

## Guarantees

- **Reproducible**: the encoding is canonical (fixed field order, sorted
  entries), so equal environments always produce equal ids.
- **Complete**: `entries` lists *every* node; there is no hidden state, no
  layer ordering to interpret. The manifest is an exact SBOM.
- **Verifiable offline**: a store holding every `blake3` referenced by the
  manifest can prove completeness without any network access.

## Archive format (`myc export`)

A gzip tar containing `manifest.json` followed by `blobs/<hash>` for every
referenced blob. `myc import` re-hashes every blob on the way in and refuses
corrupt or incomplete archives.

## OCI export (`myc export --oci`)

The reverse of ingestion: the manifest + store are rebuilt into a standard
OCI image, so any Mycel environment loads into Docker, Podman, Kubernetes,
containerd, skopeo or crane. One tar carries both formats:

```text
oci-layout                     {"imageLayoutVersion":"1.0.0"}
index.json                     OCI index -> image manifest (ref.name annotation)
blobs/sha256/<config sha>      OCI image config (env, entrypoint, cmd, workdir,
                               user, ExposedPorts, Volumes from the manifest)
blobs/sha256/<manifest sha>    OCI image manifest (config + one layer)
blobs/sha256/<layer sha>       the whole rootfs as one gzip tar layer
manifest.json                  legacy docker-save record with RepoTags
```

The layer is rebuilt deterministically (entries already sorted by path,
mtime 0 everywhere, zero gzip timestamp): exporting the same manifest twice
produces byte-identical archives. `diff_id` in the config is the sha256 of
the uncompressed layer tar, as the OCI spec requires. Hardlinks (two
entries sharing one `blake3`) are materialized as independent files.
