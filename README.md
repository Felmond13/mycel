# Mycel

[![CI](https://github.com/Felmond13/mycel/actions/workflows/ci.yml/badge.svg)](https://github.com/Felmond13/mycel/actions/workflows/ci.yml)

**Content-addressed container runtime. No daemon, no root, no image blobs.**

Mycel replaces opaque container images with *content-addressed file graphs*:
every file is stored exactly once (BLAKE3), environments are described by
small manifests (a few KB instead of hundreds of MB), and rootfs creation is
hardlink-based — zero bytes copied. Any existing Docker/OCI image can be
ingested as-is; no Dockerfile rewrite, no new ecosystem to bootstrap.

```console
$ myc run alpine:3.20 -- echo hello from mycel
hello from mycel
```

## Why

Docker's model froze four assumptions in 2013 that no longer need to be true:

| Docker | Mycel |
|---|---|
| Layered tarball images, opaque | Explicit file graph, every file hashed |
| Push/pull gigabytes per update | Diffs are the *actual* changed files |
| Tag `latest` can silently move | Manifest id = BLAKE3 of content, bit-reproducible |
| 40 copies of glibc on one host | Every file stored once, shared by hardlink |
| Root daemon always running | Single static binary, rootless user namespaces |

The manifest doubles as an exact SBOM: "which environments contain this
exact OpenSSL binary?" is a store query, not a three-week audit.

## Never used containers? 5-minute tutorial

You need zero container knowledge for this. Two concepts:

- An **environment** is a folder of files a program needs to run (its own
  `/bin`, `/lib`, `/etc` …) plus a default command. Docker calls these
  "images"; there are thousands of ready-made ones on Docker Hub.
- The **store** (`~/.mycel`) is where Mycel keeps every file of every
  environment — each unique file exactly once.

```bash
# 0. Is my machine ready? (Linux or WSL2)
myc doctor
# PASS  operating system   Linux 6.6… (WSL2)
# PASS  user namespaces    unprivileged user namespaces work (probe passed)
# PASS  content store      store at /home/you/.mycel is writable
# PASS  registry network   Docker Hub reachable over HTTPS

# 1. Get a shell inside a tiny Linux distribution called Alpine (~8 MB).
#    It is downloaded automatically the first time.
myc shell alpine:3.20
/ # cat /etc/os-release     # ← you are now inside the environment
/ # exit                    # ← and you are back

# 2. Run a single command instead of a shell
myc run alpine:3.20 -- echo hello from mycel

# 3. See what you have, and what it costs on disk
myc ls
myc stats

# 4. Get the next Alpine version and see EXACTLY what changed
myc shell alpine:3.21     # note how little is downloaded…
myc diff alpine:3.20 alpine:3.21

# 5. Explore everything in your browser instead
myc ui                    # → http://localhost:7777
```

That's the core. `myc` with no arguments recaps the most useful commands,
and every error tries to tell you what to do next.

## Superpowers (things layered images cannot do)

Every environment is an explicit list of `(path, BLAKE3 hash)` — so the
store is a database you can query:

```bash
# Exact file-level diff between any two environments: added / removed /
# modified / metadata-only, plus the true size of the *new* content.
myc diff web:v1 web:v2
# web:v2 differs from web:v1 by 12 files: 4 added, 1 removed, 7 modified,
# 0 metadata-only — 3.1 MiB of new content (1409 files unchanged)

# Reverse query: which environments contain this blob or path?
# The "which machines run the vulnerable OpenSSL" question, answered in ms.
myc which 02d34b3fc75c          # by content-hash prefix
myc which libssl                # by path substring

# Machine-readable SBOM from the manifest: every file + hash + size, and
# the real installed-package list parsed out of apk/dpkg databases.
myc sbom alpine:3.20 > sbom.json

# Per-environment sharing: how much of each env is already covered by the
# others (the reason pulling a sibling tag costs almost nothing).
myc stats
```

## No lock-in: the exit is one command

Adopting Mycel is **fully reversible**. Ingestion is two-way: any
Docker/OCI image becomes a Mycel environment, and any Mycel environment —
including ones you built or modified with `myc build` — exports back to a
standard OCI image that Docker, Podman, Kubernetes, containerd, skopeo and
crane all consume:

```console
$ myc export --oci redis:7-alpine redis-oci.tar
exported myc1-a776265c6408 as OCI image -> redis-oci.tar
  docker.io/library/redis:7-alpine (702 files, layer 15.2 MiB sha256:d46296e2ea80)
  load it anywhere: docker load < redis-oci.tar

$ docker load < redis-oci.tar
Loaded image: redis:7-alpine
$ docker run redis:7-alpine        # runs identically
```

The archive is simultaneously a valid **OCI image layout** (`oci-layout` +
`index.json` + `blobs/sha256/…`, for skopeo/podman/containerd) and a valid
**`docker save` tarball** (`manifest.json` with `RepoTags`, for `docker
load`). The rootfs is rebuilt from the manifest as a single deterministic
layer: env, entrypoint, cmd, workdir, user, exposed ports and volumes are
all carried over, and two exports of the same environment are byte-identical.

Try Mycel on one project for a week; if it is not for you, `myc export
--oci` every environment and you are back on Docker having lost nothing.
There is no migration project in either direction — that is the point.

## Web dashboard

```bash
myc ui              # → http://localhost:7777
```

A local, self-contained dashboard (no CDN, single binary) built so someone
who has never touched a terminal can use containers:

- **Apps** — a one-click catalog (Nginx, PostgreSQL, Redis, MongoDB, Python,
  Node). Three verbs: *Install*, *Start*, *Stop*. Running web apps get an
  "Open in browser" link (containers share the host network, so nginx is
  just `localhost:8080`). Ports sort themselves out: if an app's usual port
  is already busy, the dashboard automatically gives the app its own
  private network and publishes it on the nearest free port — "Port 6379
  was busy — your app is available on localhost:6380 instead" — no error,
  no flag, nothing to learn (needs the `passt` package; without it the old
  clear refusal stays). The Customize dialog also offers the private
  network explicitly: one toggle, plus an editable "available on
  localhost:…" field per port. Data persists by default: every data directory an
  image declares (redis `/data`, postgres…) is automatically mapped to a
  stable named volume (`redis-data`), so stop/start never loses anything —
  nobody needs to know what a volume is. The Customize dialog has a simple
  "Keep this app's data between restarts" toggle, or a folder picker fo
  users who want the data in a folder of their computer.
- **Running** — everything you started, with green/red status dots,
  uptime, live logs, restart and stop. Containers run as child processes of
  the UI server (`myc run` re-invoked per container, own process group,
  logs capped at 2 MiB) — a crashing app can never take the dashboard down.
- **Get any app** — fetch software by name from any registry (Docker Hub,
  GHCR, Quay…); Mycel stores it its own way: deduplicated,
  content-addressed files, not images.
- **Library** — the software installed locally: file trees, SBOMs, run
  commands, pin/remove. Each app can be shared (as a portable `.mycel`
  file, as a standard Docker image that `docker load` and any cloud
  accept, or through a hub) and **deployed to your own server** in one
  dialog — type `user@host`, only the missing files travel, and the app
  can be started there right away (the web face of `myc deploy`).
- **Stacks** — orchestration from the browser: build a multi-service
  project with a form (apps, env vars, start-after dependencies), no TOML
  in sight unless you flip the "Advanced" toggle. Stacks are plain
  `mycel.toml` files under `~/.mycel/stacks/`, started in dependency order.
  A stack with `[network] mode = "pod"` gets a "private network" badge: its
  apps meet on localhost, only the listed ports are reachable from the
  computer, and unexposed services read "internal only".
- **Advanced** — the power-user pages: store-wide dedup visualization,
  file-level compare, reverse file search, doctor.

Everything is backed by the JSON API documented in `docs/web-api.md`.
Interactive shells (`myc shell`) stay in the terminal; the UI runs
services and one-shot commands.

*(screenshot placeholder: docs/ui-screenshot.png)*

## Install

Requires Linux (or WSL2 on Windows) and Rust stable.

```bash
cargo build --release          # produces target/release/myc (~7 MB, UI included)
sudo cp target/release/myc /usr/local/bin/
```

Container execution uses unprivileged user namespaces (same mechanism as
Podman rootless). Most distributions enable them by default.

## Windows & macOS

**Windows — shipped.** `myc.exe` is one self-contained binary that
transparently proxies every command to a Linux twin inside WSL2: the ~9 MB
Linux build of `myc` is embedded in the .exe and provisioned into
`~/.mycel-bin/myc` on first run (sha256-checked on every command, so
upgrading the .exe self-heals the twin). There is exactly **one store**,
inside WSL (`~/.mycel` on ext4, where hardlink materialization stays free),
so ingest, containers, builds, lazy streaming, the hub and the web UI all
behave identically to Linux. `myc.exe ui` runs the server inside WSL and
WSL2's localhost forwarding makes `http://localhost:7777` reachable from
Windows browsers.

Double-clicking `myc.exe` in Explorer starts the dashboard: it launches
the proxied `ui` server on :7777 (or reuses one already listening) and
opens your default browser on it; the console window stays open while the
dashboard runs, and closing it (or Ctrl-C) stops the server. From an
interactive shell, bare `myc` still prints the usual welcome text —
detection uses `GetConsoleProcessList`: a double-click gives myc.exe a
console of its own, a shell stays attached alongside.

```powershell
PS> myc.exe doctor    # WSL2 present? distro? twin provisioned? version match?
PS> myc.exe run alpine:3.20 -- /bin/echo hello-from-windows
hello-from-windows
```

On Windows, `myc doctor` first checks the WSL2 chain (with actionable
fixes: `wsl --install -d Ubuntu`, `$MYCEL_WSL_DISTRO`, `$MYCEL_WSL_BINARY`)
and then runs the Linux-side checks through the twin. Escape hatch:
`--no-proxy` (or an explicit Windows `--store` path) runs the command
natively on the Windows host — store-only commands work
(`ls`/`inspect`/`stats`/`ingest`/`export`/`import`/`gc`/…), containers
still need Linux. Build: `scripts/build-windows.ps1`. Test:
`scripts/e2e-windows.ps1`.

**macOS — planned, not yet available.** The code compiles (all platform
fixes are `cfg`-based) but the runtime honestly returns "not yet
available". The plan — a lightweight pinned-kernel Linux VM via
Virtualization.framework with virtiofs store sharing, same transparent-twin
UX as Windows — is written up in
[docs/desktop-architecture.md](docs/desktop-architecture.md).

## Quick start

```bash
# Ingest any OCI image — Docker Hub, GHCR, Quay, private registries
myc ingest postgres:16

# Run it, rootless, no daemon
myc run alpine:3.20 -- sh -c 'echo hi from $(hostname)'

# Bind mounts, env vars, hostname
myc run -v ./data:/var/lib/app -e MODE=dev --hostname web alpine:3.20 -- sh

# Named volumes: persistent app data managed by Mycel (like Docker's).
# A source without a slash is a volume name, created on first use unde
# ~/.mycel/volumes/<name>/data — the data survives container restarts.
myc run -v mydata:/data redis:7-alpine
myc volume ls                  # every volume with its size
myc volume inspect mydata      # host path (and the \\wsl.localhost twin on WSL)
myc volume rm mydata           # delete the data

# Networking: zero concepts by default (containers share the host network,
# a service on port P is just localhost:P). Optional isolation in one flag:
# the app gets its own private network — outbound works, and only the ports
# you publish are opened on the host. Rootless, via pasta (package `passt`).
myc run --net isolated redis:7-alpine          # publishes the image's
                                               # exposed ports 1:1
myc run -p 6380:6379 redis:7-alpine            # host 6380 → container 6379
                                               # (-p implies --net isolated)
myc run --net none alpine:3.20 -- sh           # no connectivity at all

# What's in the store?
myc ls
myc stats            # dedup ratio, physical vs logical size
myc inspect alpine:3.20 --files

# Offline guarantee: prove every referenced blob is present locally
myc pull alpine:3.20

# Air-gapped transfer (USB stick, isolated networks)
myc export alpine:3.20 -o img.tar.gz
myc import img.tar.gz          # on the other machine

# Exit hatch: back to a standard OCI image, for Docker/Podman/Kubernetes
myc export --oci alpine:3.20 alpine-oci.tar
docker load < alpine-oci.tar   # anywhere docker runs

# Integrity: re-hash every blob against the manifest
myc verify alpine:3.20

# Space management
myc pin alpine:3.20   # GC will never touch its blobs
myc rm old-app:1 && myc gc
```

## Deploy to any Linux server

`myc deploy` takes an environment from your machine to any Linux box you
can ssh into — a 5€ VPS, a bare-metal server, a Raspberry Pi. No Docker on
the server, no registry in the middle, no daemon, no root: the only
requirements are sshd and a `myc` binary (and if the server doesn't have
one, the first deploy offers to upload it — one static file).

```console
$ myc deploy web:v2 deploy@vps.example.com --run
deployed web:v2 → deploy@vps.example.com — 3 files sent (1.2 MiB), 1240 already present — done in 4.3s
started web:v2 on deploy@vps.example.com (pid 51230, log: ~/.mycel/deploys/web_v2.log) — port 8080
```

Because content is addressed by hash, the deploy negotiates like the hub
does: one round-trip asks the server which files it is missing, and *only
those* cross the wire. The first deploy sends the environment; every
deploy after that sends just the files you actually changed — typically a
few hundred KB, in about a second, even for an environment that is
hundreds of MB. Re-deploying an unchanged ref sends exactly zero bytes.

```bash
myc deploy web:v2 ssh://deploy@vps:2222     # custom ssh port
myc deploy web:v3 deploy@vps --restart      # stop web:v3's previous instance, start the new one
myc deploy web:v2 deploy@vps --run -p 80:8080 -v webdata:/data --net isolated
myc deploy web:v2 deploy@vps --no-bootstrap # never upload a binary
```

Everything rides your existing ssh setup — keys, agent, `~/.ssh/config`
aliases, jump hosts. The transfer is verified end-to-end (the server
re-hashes every file and re-derives the manifest id), and the ref only
becomes visible on the server once every file is present, so a killed
deploy can never leave a half-environment. Details: [docs/deploy.md](docs/deploy.md).

## CI in seconds

GitHub-hosted runners are ephemeral, so Docker re-downloads the same layers
on every single run. Mycel's store is a plain directory of content-addressed
files: cache it with `actions/cache` and warm runs pull **only the files
that changed** — usually nothing.

```yaml
steps:
  - uses: Felmond13/mycel/action@v0.2.0   # installs myc + caches the store

  - run: myc pull postgres:16   # warm cache: verifies hashes, ~0 B downloaded
  - run: myc run postgres:16 -- postgres --version
```

First run ingests as usual; every run after that starts with all known files
already local. Full guide with the before/after numbers:
[docs/ci.md](docs/ci.md).

## Team hub: push and pull only the delta

Any Mycel store can be served over HTTP for a whole team. Because content
is addressed by hash, synchronizing two stores reduces to one round-trip —
"which of these hashes are you missing?" — and only that delta ever crosses
the wire.

```bash
# On a shared machine (CI box, LAN server) — reads open, writes tokened:
myc hub serve --port 9600 --token SECRET

# From any dev machine:
export MYCEL_HUB=http://hub:9600
myc push web:v2                 # manifest + only the blobs the hub lacks
# pushed web:v2 — 27 blobs of 81 uploaded (7.2 MiB of 7.5 MiB) in 62.7ms

myc pull web:v2                 # only the blobs *you* lack
# pulled web:v2 — 80 blobs of 80 downloaded (7.4 MiB of 7.4 MiB) in 434.5ms
```

Measured on this machine (`scripts/bench.sh`, alpine:3.20/3.21, localhost
hub): re-pushing an unchanged environment uploads **0 blobs / 0 bytes in
~22 ms**; pushing alpine:3.21 when the hub already has 3.20 uploads only
the 27 changed blobs. `docker pull alpine:3.20` on the same machine:
3.6 s.

The protocol is small and honest: JSON + raw blobs under `/api/v1/`
(`manifests`, `refs`, `blobs`, and `POST /missing` for the delta
negotiation). The server re-hashes every uploaded blob and re-derives every
manifest id — a client can never poison the store.

## Lazy streaming: run before you download

The hub also unlocks `--lazy`: start an environment whose blobs are *not*
local, and stream files on demand as the application actually touches them.

```bash
myc run --lazy --from http://hub:9600 alpine:3.20 -- echo hello
# myc: lazy rootfs ready in 60ms — 0 files local, 87 deferred (7.4 MiB)
# hello
# myc: lazy run done — fetched 2 of 87 deferred files on demand (1.4 MiB)
```

Measured cold start on this machine, empty store, time to first output:
**~190 ms** (vs 3.6 s for `docker pull` alone) — because `echo` only needs
busybox and the musl loader, only those 2 of 87 files were fetched. Every
fetched file lands in the content store, so it is cached forever and shared
with every other environment; the second lazy run fetches **0 files** and
starts in ~44 ms.

How: the manifest (KBs) is fetched first; directories, symlinks and
already-local files are materialized instantly (hardlinks, as always);
each missing file becomes a symlink into `/.myc-lazy`, a read-only FUSE
filesystem (mounted unprivileged, pure-Rust `fuser`) that answers `stat`
from the manifest and fetches blob content from the hub on first `open`.
Image content stays immutable — writes go to the real rootfs directories
exactly as in a normal run.

## Build images natively — no Dockerfile, no daemon

`myc build` produces a manifest directly from a small declarative file,
`mycel-build.toml`. No layers are created and none are rebuilt: files are
hashed straight into the content store, so a rebuild after changing one
file stores exactly that one file.

```toml
[build]
base = "alpine:3.20"        # omit to build from scratch (empty rootfs)
name = "mon-app:1.0"        # or override with: myc build -t mon-app:1.1

[[build.add]]
source = "./dist/mon-app"    # file OR directory (recursive), relative to this file
dest = "/app/mon-app"
mode = 0o755                 # optional (default: preserve source mode)

[config]
entrypoint = ["/app/mon-app"]
cmd = ["--serve"]
env = ["PORT=8080"]          # merged over the base env (same key replaces)
workdir = "/app"
```

Need to run commands during the build (the `RUN` equivalent)? Add ordered
exec steps — they run in a real rootless container, and the changed files
are folded back into the manifest:

```toml
[[build.run]]
command = ["/bin/sh", "-c", "apk add --no-cache curl"]
env = ["http_proxy="]        # optional, this step only
```

```console
$ myc build -f mycel-build.toml
  add dist/app -> /usr/local/bin/app: 1 file(s), 36 B new
  step 1/1: /bin/sh -c apk add --no-cache ca-certificates
  step 1 ok: +446 added, ~4 modified, -0 removed, 283.0 KiB to store, 1.4s
myc1-f96313114877  demo:1
  239 files, 7.7 MiB logical, 283.0 KiB added to store (dedup saved 7.4 MiB)
```

Builds are reproducible: pass `--timestamp <rfc3339|unix>` (or set
`SOURCE_DATE_EPOCH`) and the same inputs produce the *same manifest id*,
bit for bit. See `docs/build-format.md` for the full format and
`examples/build-hello/` for a working example.

## Multi-service projects

Declare services in `mycel.toml`, start everything with one command:

```toml
[project]
name = "shop"

[services.db]
image = "postgres:16"
env = ["POSTGRES_PASSWORD=dev"]

[services.api]
image = "ghcr.io/acme/api:1.2"
depends_on = ["db"]
binds = ["./data:/var/lib/app"]
```

```bash
myc up            # resolves images, starts in dependency order, foreground
```

### Give the stack its own private network ("pod" mode)

By default services share the host network. Add a `[network]` section and
the whole stack lives in its own little machine instead: services talk to
each other on **localhost** (the api reaches postgres at `127.0.0.1:5432`
with zero configuration), and the host only sees the ports you list —
everything else is unreachable from outside:

```toml
[network]
mode = "pod"              # default: "host" (the historical behavior)
ports = ["8080", "15432:5432"]   # "HOST" or "HOST:CONTAINER"; the rest stays internal
```

```console
$ myc up
Stack "shop" is running in its own private network — reachable from this
machine: localhost:8080, localhost:15432 → port 5432 inside
```

How it works, rootless as always: `myc up` starts a tiny durable *holder*
process that owns one user + network namespace pair for the stack, attaches
a **single pasta instance** publishing the `[network]` ports, and every
service joins those namespaces (`setns`) instead of creating its own. Two
stacks can both run "their" redis on 6379 without ever colliding.

Lifecycle, including the unhappy paths: when every service has exited (or
on Ctrl-C) `myc up` stops the holder itself; if `up` is killed outright the
holder dies with it (`PR_SET_PDEATHSIG`) and pasta exits as soon as the
namespace empties, so nothing leaks. `myc down` cleans up whatever a brutal
stop left behind (holder, pasta, the state record under `<store>/pods/`).
`myc up --net pod` / `--net host` overrides the file without editing it.

If pasta (package `passt`) is missing, a project file asking for pod mode
falls back to the host network with a loud warning — the stack still runs —
while an explicit `myc up --net pod` fails with the install hint. Try it:
`examples/pod/mycel.toml` is a web + redis pair where redis is internal-only.

The dashboard follows the same file: a stack with `mode = "pod"` shows a
"private network" badge, the published ports, and "internal only — reachable
by the other apps in this stack" for everything not exposed.

## How it works

```
crates/
  myc-manifest   The manifest format: file graph + runtime config,
                 canonical JSON, id = BLAKE3 of the encoding
  myc-store      Content-addressed store (~/.mycel): blobs named by hash,
                 atomic writes, refs, pins, mark-and-sweep GC
  myc-oci        OCI Distribution client: token auth, manifest lists,
                 sha256-verified layer streaming, whiteout handling
  myc-run        Rootfs materialization (hardlinks out of the store) and
                 rootless execution: user/mount/PID/UTS/IPC namespaces,
                 pivot_root, minimal /dev, /proc
  myc-compose    mycel.toml parser + dependency-ordered local orchestrato
  myc-query      Store-wide queries: diff, reverse lookup (which), SBOM
                 (apk/dpkg parsing), dedup analytics, environment docto
  myc-web        Local web dashboard: axum JSON API + embedded single-file
                 frontend (no build toolchain, no CDN)
  myc-hub        Team hub: the store served over HTTP (axum) + blocking
                 client + push/pull delta negotiation (POST /missing)
  myc-lazy       Lazy streaming rootfs: read-only FUSE blob filesystem
                 that faults file contents in from a hub on first open
  myc-build      Native image builds from mycel-build.toml: adds hashed
                 straight into the store, exec steps in rootless
                 containers with copy-materialized rootfs + rescan
  myc-backend    Execution backends: native Linux namespaces, transparent
                 WSL2 twin proxying on Windows, macOS VM scaffold
  myc-cli        The `myc` binary
```

Ingestion streams each layer tar once: file contents go straight into the
store (deduplicated on the fly), while an in-memory tree applies OCI
whiteouts and shadowing. The result is a flat manifest — layers are gone.

Running an environment materializes a rootfs in milliseconds by hardlinking
every file from the store, then enters fresh namespaces, pivot_roots and
execs. Ten containers from ten images share every common file on disk and in
page cache.

## Security model

- **Rootless by construction**: the runtime never needs root; uid 0 inside a
  container is your own uid outside. When `/etc/subuid` ranges and the
  `newuidmap`/`newgidmap` helpers are available (package `uidmap` — the same
  mechanism Podman uses), containers additionally get uids 1-65536 mapped to
  your subordinate range, so images that switch to a service use
  (`USER postgres`, chown, su) work; without them Mycel falls back to the
  single-uid map and the dashboard explains failures caused by it.
  `MYCEL_SINGLE_UID=1` forces the fallback.
- **Verified supply chain**: layer downloads are sha256-checked against
  registry digests; every stored file is BLAKE3-addressed; `myc verify`
  re-checks at rest. A manifest id pins an environment bit-for-bit.
- **No daemon**: no privileged socket to hijack, nothing running when
  nothing runs.

## Testing

```bash
scripts/dev.sh test    # unit + integration tests (real hub server on
                       # localhost, real FUSE mounts, real containers)
scripts/e2e.sh         # end-to-end against real registry images:
                       # ingest/run/export/import/gc + diff/which/sbom/
                       # doctor + web UI smoke test + hub roundtrip
                       # (pull/push/delta) + lazy cold start
scripts/bench.sh       # the numbers: hub pull, lazy cold start,
                       # incremental push, docker comparison
```

## Status and roadmap

Working today: OCI ingestion (Docker Hub / GHCR / Quay / private v2
registries, gzip + zstd layers, multi-arch indexes), content store with
GC/pins/verification, rootless execution on Linux & WSL2, bind mounts,
named volumes with automatic data persistence (image `Volumes` are parsed
at ingest and mapped to stable volumes by the dashboard),
host networking plus opt-in rootless network isolation with published
ports (`--net isolated` / `-p`, backed by pasta) and automatic port-conflict
resolution in the dashboard, stack "pod" networking (`[network] mode =
"pod"` in `mycel.toml`: one private network per stack, services meeting on
localhost, only the listed ports published — see *Multi-service projects*),
export/import archives, multi-service `up`/`down`, exact
environment diffs, reverse blob/path queries, SBOM generation, per-env
dedup analytics, environment doctor, interactive `shell`, the local
web dashboard (`myc ui`), the team hub (`myc hub serve` / `push` /
`pull --from`), lazy streaming runs (`myc run --lazy`), and native
builds (`myc build`, declarative adds + exec steps, no Dockerfile).

Not yet implemented (by design, in order):

1. macOS micro-VM backend (today: Windows ships the transparent WSL2
   proxy — see *Windows & macOS*; macOS is architecture-only,
   `docs/desktop-architecture.md`)
2. Hub: TLS termination and per-user tokens (today: one write token,
   put nginx/caddy in front for TLS)

## License

Mycel is source-available under the [Business Source License 1.1](LICENSE)
(SPDX: `BUSL-1.1`), owned by **Noureddine BOUKADOUM**.

In short: you may read, use, and modify the code freely for personal use and
internal non-commercial use. What you may not do without a commercial license
from the owner is redistribute, republish, or duplicate Mycel or a derivative
of it (beyond the GitHub fork strictly needed to submit a contribution), or
offer it as a commercial or competing product or service.

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) (note: it
includes an assignment of rights on contributions).

For a commercial license, contact: **noureddine.boukadoum@gmail.com**.

On the Change Date, **July 22, 2030**, all of this code automatically becomes
available under the Apache License, Version 2.0.
