# The `mycel-build.toml` format

`myc build [-f mycel-build.toml] [-t NAME:TAG] [--timestamp T]` produces a
Mycel manifest natively: no Dockerfile, no daemon, no layers. The build file
has three parts — the `[build]` section, optional exec steps, and an
optional `[config]` section.

## `[build]`

```toml
[build]
base = "alpine:3.20"   # optional: any image reference or local ref/id.
                       # Omitted = build from scratch (empty rootfs).
name = "mon-app:1.0"   # result name. `myc build -t OTHER:TAG` overrides it.
```

The base is resolved through the local store first and auto-ingested from
its registry when missing (exactly like `myc run`).

## `[[build.add]]` — files and directories

```toml
[[build.add]]
source = "./dist/mon-app"  # file OR directory (recursive),
                           # relative to the toml file
dest = "/app/mon-app"      # absolute path inside the image
mode = 0o755               # optional; default: preserve the source mode
                           # (directories always get 755)
uid = 0                    # optional, default 0
gid = 0                    # optional, default 0
```

Semantics:

- Every added file is BLAKE3-hashed into the content store; files the store
  already has cost zero bytes ("dedup saved" in the output).
- Missing parent directories are created as `dir` entries, mode 755.
- Adds merge **over** the base: the add wins on path conflicts, and adding
  a file over a directory removes the whole subtree (the same shadowing
  rule OCI layers use).
- Directory adds record symlinks as symlinks; sockets/FIFOs/devices in the
  build context are ignored.

## `[[build.run]]` — exec steps (the `RUN` equivalent)

```toml
[[build.run]]
command = ["/bin/sh", "-c", "apk add --no-cache curl"]
env = ["http_proxy="]      # optional extra env for this step only
```

Steps run **after all adds**, in file order. Each step:

1. Materializes the current build state (base + adds + previous steps) into
   a temporary rootfs — with **copies**, not the usual hardlinks, so a step
   that modifies files in place can never touch the store's immutable
   blobs.
2. Executes the command in a rootless container (host network, so package
   managers work; the merged `[config]` env and workdir apply, like
   Dockerfile `ENV`/`WORKDIR` for `RUN`).
3. On exit 0, rescans the rootfs: new/changed/removed files are folded back
   into the manifest and new content is stored. A non-zero exit aborts the
   build (the step's output goes straight to your terminal).

Runtime artifacts never leak into the image: `/proc`, `/dev`, `/tmp`,
`/.oldroot`, `/etc/resolv.conf` and `/etc/hosts` are excluded from the
scan; their pre-step entries (from the base) are preserved verbatim.

Ownership: a rootless build maps your uid to 0, so files a step creates or
modifies are recorded as `uid 0 / gid 0` (root inside the container —
exactly how the step itself saw them). Files the step did **not** touch
(same size and hash) keep their original uid/gid/mode from the base; a
`chmod` without a content change is picked up as a metadata modification.

## `[config]` — runtime configuration

```toml
[config]
entrypoint = ["/app/mon-app"]  # optional
cmd = ["--serve"]              # optional
env = ["PORT=8080"]            # merged over the base env (same key replaces)
workdir = "/app"               # optional
user = ""                      # optional
```

The base config is the default; every field you set replaces it, except
`env`, which merges key-by-key (base order preserved, new keys appended).

## Reproducible builds

`created` is part of the canonical manifest, so the timestamp must be
pinned for identical ids:

```console
$ myc build --timestamp 2026-01-01T00:00:00Z     # RFC 3339
$ myc build --timestamp 1767225600               # unix seconds
$ SOURCE_DATE_EPOCH=1767225600 myc build         # reproducible-builds.org
```

Priority: `--timestamp` > `SOURCE_DATE_EPOCH` > current time. With a pinned
timestamp, the same base + same inputs produce the **same manifest id**
(directory adds are walked in sorted order for this reason). Note that run
steps are only as reproducible as the commands they run — `apk add` fetches
whatever the mirror serves today.

## Output

```console
$ myc build -f mycel-build.toml --timestamp 2026-01-01T00:00:00Z
  add dist/app -> /usr/local/bin/app: 1 file(s), 36 B new
  step 1/1: /bin/sh -c apk add --no-cache ca-certificates
  step 1 ok: +446 added, ~4 modified, -0 removed, 283.0 KiB to store, 1.4s
myc1-f96313114877  demo:1
  239 files, 7.7 MiB logical, 283.0 KiB added to store (dedup saved 7.4 MiB)
```

The result is an ordinary manifest: `myc run`, `myc push`, `myc export`,
`myc diff` all work on it. Its `origin` records the build file
(`build://mycel-build.toml`).

A complete working example lives in `examples/build-hello/`.
