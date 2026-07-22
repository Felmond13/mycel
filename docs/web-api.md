# Dashboard JSON API

`myc ui [--port 7777]` serves the dashboard at `http://localhost:<port>/`
and a JSON API under `/api/`. The server binds to `127.0.0.1` only. Errors
are always `{"error": "message"}` with a 4xx/5xx status.

References (`{id}`, `a`, `b`) accept anything `myc` accepts on the command
line: manifest ids, unique id prefixes, or ref names. They are resolved
through the store's reference table — never used as filesystem paths — and
path-like inputs are rejected with `400`.

| Method | Path | Description |
|---|---|---|
| GET | `/api/envs` | List environments (id, name, refs, files, logical size, pinned) |
| GET | `/api/envs/{id}` | Detail: runtime config, full entry list, missing-blob count, copy-paste run command |
| DELETE | `/api/envs/{id}` | Remove the manifest (blobs reclaimed by GC) |
| GET | `/api/envs/{id}/sbom` | SBOM: apk/dpkg packages + every file with BLAKE3 hash |
| POST | `/api/envs/{id}/pin` | Protect from GC |
| POST | `/api/envs/{id}/unpin` | Remove protection |
| GET | `/api/stats` | Store stats: blobs, physical/logical bytes, dedup ratio, per-environment sharing |
| GET | `/api/diff?a=REF&b=REF` | File-by-file diff report between two environments |
| GET | `/api/which?q=QUERY` | Reverse query by hash prefix (≥6 hex chars) or path substring |
| POST | `/api/gc` | Run garbage collection; returns blobs deleted and bytes freed |
| GET | `/api/doctor` | Environment health checks (PASS/FAIL + fixes) |
| GET | `/api/capabilities` | What this machine supports: `{"network_isolation": bool}` (true when pasta, from the `passt` package, is installed) |
| POST | `/api/ingest` | Body `{"image": "alpine:3.20"}`; returns `{"job": N}` immediately |
| GET | `/api/jobs/{n}` | Poll ingestion progress: status (`running`/`done`/`failed`), layers done/total, files, bytes, `created_at` |
| GET | `/api/jobs` | The 20 most recent ingest jobs (newest first, each with its `id`) — an in-memory session history |

## Sharing

Environments travel as portable `.mycel` archives (gzipped tar: the
manifest plus every blob it references — the same format as `myc export` /
`myc import`, produced by the shared `myc_store::archive` module), o
directly between stores through a hub (`myc hub serve`), in which case only
the blobs the receiving side is missing cross the wire.

| Method | Path | Description |
|---|---|---|
| GET | `/api/export/{ref}` | Download the environment as a `.mycel` archive. Streams with `Content-Disposition: attachment` and a clean filename (`redis-7-alpine.mycel`). `400` if blobs are missing locally. |
| POST | `/api/import` | Import a `.mycel` archive sent as the raw request body (`application/octet-stream`, 8 GiB cap). Every blob is re-hashed on the way in. Returns `{"id", "name", "refs", "blobs"}`; `400` with a plain-language error for anything that is not a valid archive. |
| POST | `/api/hub/push` | Body `{"ref": "...", "url": "http://hub:9600", "token"?: "..."}`. Push an installed environment to a hub; only blobs the hub is missing are uploaded. Returns `{"ref", "manifest_id", "blobs_total", "blobs_uploaded", "blobs_already_present", "bytes_total", "bytes_uploaded", "elapsed_ms"}`. |
| POST | `/api/hub/pull` | Body `{"ref": "...", "url": "http://hub:9600"}`. Fetch an environment from a hub; only blobs this store is missing are downloaded. Tries the ref as given, then its canonical docker-style name. Returns `{"ref", "manifest_id", "blobs_total", "blobs_downloaded", "blobs_already_present", "bytes_total", "bytes_downloaded", "elapsed_ms"}`. |

## Containers

Containers started over the API run as child processes of the UI server:
the server re-invokes its own binary (`myc run <manifest-id> …`) in a
dedicated process group, with stdout/stderr captured to a log file unde
`<store>/ui-logs/` (capped at 2 MiB). The reference must already be in the
store — the API never triggers a registry pull implicitly.

The registry survives server restarts: it is persisted to
`<store>/ui-containers.json` on every change, and on startup the serve
reconciles it against `/proc` (pid **and** kernel start time must match).
Containers still alive are re-adopted — listed, stoppable, logs readable
(output produced while the server was down is lost) — and containers that
died in the meantime are reported as exited with `exit_code: null` (thei
real code was unobservable).

Starting a container preflights its TCP ports (the image's `ExposedPorts`
plus the user-declared `port`) against the host's listening sockets. A
conflict no longer refuses outright: when network isolation is available
on the machine (pasta installed — see `/api/capabilities`), the server
automatically starts the container in its own network namespace with the
busy port published on the nearest free host port, and the response
carries a one-sentence `port_note` ("Port 6379 was busy — your app is
available on localhost:6380 instead") plus the real mappings in `ports`.
Without pasta, the old behavior stays: `409` with a plain-language error
(now including the `passt` install hint) before anything runs. Isolation
can also be requested explicitly with `isolate_network: true` and
per-port host choices in `ports`; busy choices are bumped the same way.
`network` is `host` or `isolated` on every container object, `ports` is
`[{"host": n, "container": n}]` (empty on host-network containers), and
`port` is always the host-side port that actually answers — the "Open in
browser" link needs no extra logic. `/restart` re-plans the network the
same way (original ports are kept when still free).

If a container still exits non-zero within ~10 s of starting, the
server scans its log tail for known signatures (e.g. "address already in
use") and fills `failure_hint` with a human explanation. `stopped_by_user`
is true when the exit followed an explicit stop request (Stop button,
restart, stack down): the resulting 143/137 exit codes are a successful
shutdown, not an error, and the UI shows them as "stopped by you".

| Method | Path | Description |
|---|---|---|
| POST | `/api/containers` | Start a container. Body: `{"reference": "nginx:latest", "name"?: str, "command"?: [str], "env"?: ["K=V"], "workdir"?: str, "map_user"?: "UID[:GID]", "port"?: n, "keep_data"?: bool, "data_folder"?: str, "isolate_network"?: bool, "ports"?: [{"host": n, "container": n}]}`. Returns the container object (plus `port_note` when a busy port was remapped); `409` when a required port is taken **and** isolation is unavailable. |
| GET | `/api/containers` | List all containers: `{"containers": [{id, name, reference, command, env, workdir, status, running, exit_code, stopped_by_user, failure_hint, pid, port, network, ports, stack, service, started_at, finished_at, cpu_percent, memory_bytes, disk_bytes, pids_count}]}` |
| GET | `/api/containers/{id}/processes` | Live process tree: `{"processes": [{pid, name, cpu_percent, memory_bytes}], "running": bool}`, sorted by memory descending |
| GET | `/api/containers/{id}/data` | The container's data mounts: `{"data": [{container_path, host_path, windows_path, kind, volume, size_bytes, read_only}]}`. `kind` is `volume` (managed named volume) or `folder` (user bind); `windows_path` is the `\\wsl.localhost\…` twin when the server runs inside WSL, else `null`. |
| GET | `/api/containers/{id}/logs?offset=N` | Tail the log from byte `N` (max 256 KiB per call). Returns `{data, next, size, status, running, exit_code}` — poll with `offset=next`. |
| POST | `/api/containers/{id}/stop` | SIGTERM the container's process group, SIGKILL after 5 s. Blocks until dead; returns the final container object. |
| POST | `/api/containers/{id}/restart` | Stop (if running) and start a fresh container with the same spec; returns the new object. |
| DELETE | `/api/containers/{id}` | Forget an exited container and delete its log. `400` if still running. |

`status` is `running`, `stopping`, `exited(code)` or plain `exited` (a
container that ended while the server was down); a container killed by
signal N reports `exited(128+N)`. `port` is the host-side service port —
the UI uses it for the "Open in browser" link. On host-network containers
it is the user-declared hint (a service on port P is `localhost:P`); on
isolated containers it is the published host port for the declared
service port, remapped or not.

Resource metrics come from `/proc` (rootless runs cannot rely on cgroup
delegation): the container's process group is enumerated from
`/proc/*/stat`, `memory_bytes` sums VmRSS across the tree, and
`cpu_percent` is the utime+stime delta between two polls as a percentage
of **one core** (values above 100 mean multiple cores; the first poll afte
a start reports 0 because there is no delta yet). `disk_bytes` is a lazily
cached `du` of the container's rootfs, refreshed at most every 30 s. On
exited containers all four fields are `null`.

### Data persistence

Starting a container maps every data directory its image declares (OCI
`Volumes`, stored on the manifest as `config.volumes`) to a stable *named
volume* under `<store>/volumes/<name>/data`, derived from the containe
name (`redis` + `/data` → `redis-data`, `mongo` → `mongo-db` +
`mongo-configdb`). Same name, same volume: data survives stops, restarts
and removals of the container. `keep_data: false` opts out (nothing
persists), and `data_folder: "/abs/path"` uses a folder of the compute
instead of a managed volume (one declared volume maps to the folde
itself, several map to subfolders). Restart re-uses the container's
original binds, so persistence follows automatically.

## Volumes

| Method | Path | Description |
|---|---|---|
| GET | `/api/volumes` | Every named volume: `{"volumes": [{name, size_bytes, created_at, created_for, path, windows_path, in_use, used_by}]}`. `used_by` lists the *running* containers currently mounting it. |
| DELETE | `/api/volumes/{name}` | Delete a volume and all of its data. `409` while a running container uses it; `404` if it does not exist. |

## Stacks

A stack is a `mycel.toml` project owned by the UI, stored unde
`<store>/stacks/<name>/mycel.toml` (same format as `myc up`). Stack names
match `[a-z0-9][a-z0-9_-]{0,31}`.

| Method | Path | Description |
|---|---|---|
| GET | `/api/stacks` | List stacks with service/running counts |
| GET | `/api/stacks/{name}` | Detail: canonical TOML, dependency `order`, per-service config + `installed` + latest container `state` |
| PUT | `/api/stacks/{name}` | Create/update. Body is either `{"toml": "..."}` (validated) or `{"services": {"web": {"image": …, "command"?, "env"?, "depends_on"?, "workdir"?}}}` |
| DELETE | `/api/stacks/{name}` | Stop the stack's containers and delete it |
| POST | `/api/stacks/{name}/up` | Start every service in dependency order as managed containers; `409` with the list of images if some are not installed yet. Service `binds` accept named volumes (`dbdata:/var/lib/postgresql`), and declared image volumes not covered by an explicit bind are auto-mapped to stable `<stack>-<service>-<suffix>` volumes. |
| POST | `/api/stacks/{name}/down` | Stop every container belonging to the stack |
| POST | `/api/stacks/{name}/services/{svc}/restart` | Restart one service from the current stack file |

Interactive containers (`myc shell`) still belong in a terminal — the API
runs non-interactive services and commands (stdin is `/dev/null`).
