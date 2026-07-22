# `myc deploy` — ship environments over plain SSH

Deploy any environment from your local store to any Linux server you can
ssh into, transferring only the files the server does not already have.

```bash
myc deploy <ref> <target> [options]
```

- `<ref>` — a local ref name (`web:v2`) or manifest id.
- `<target>` — `user@host`, `host`, or `ssh://user@host[:port]`.
  Anything your `~/.ssh/config` resolves (aliases, jump hosts, keys,
  agent) works unchanged.

| option | meaning |
|---|---|
| `--run` | start the environment on the server after the transfer (detached, default manifest command) |
| `--restart` | stop the previous instance of the same ref first, then start |
| `-p HOST:CONTAINER` | ports for the remote `myc run` (repeatable) |
| `-v SRC:DST[:ro]` | mounts/volumes for the remote `myc run` (repeatable) |
| `--net MODE` | network mode for the remote `myc run` (`host`, `isolated`, `none`) |
| `--myc-path PATH` | where `myc` lives on the server (default: `myc` in PATH, then `~/.local/bin/myc`) |
| `--no-bootstrap` | fail instead of uploading a `myc` binary when the server has none |

## What happens

1. **Bootstrap check** — `myc` probes for a remote `myc` binary. If there
   is none, it uploads the binary it is running from to
   `~/.local/bin/myc` (one static file, `chmod +x`, verified with
   `--version`). `--no-bootstrap` turns this into an error instead.
2. **Negotiation** — the local side starts `myc _serve-deploy` on the
   server through ssh and sends the manifest (a few KB of JSON). The
   server answers with the list of content hashes its store is missing.
3. **Transfer** — only those blobs are streamed, each prefixed by a
   one-line JSON header (`hash`, `size`, `mode`). The server re-hashes
   every blob as it lands; a corrupt transfer is rejected, never stored
   under the expected name.
4. **Commit** — manifest and ref are registered only after every blob is
   present, so the server never advertises a half-deployed environment.
   The final message carries the verified manifest id back.
5. **Run (optional)** — with `--run`/`--restart`, the server starts
   `myc run <ref>` detached, with a pidfile and log under
   `~/.mycel/deploys/`.

Steps 2–4 are the same negotiation the team hub uses
(`Store::missing_blobs`), spoken over a pipe instead of HTTP — four
message kinds, one JSON object per line, raw bytes in between (see
`crates/myc-hub/src/deploy.rs` for the exact frames).

## Why it is fast

The store is content-addressed, so "what changed" is computed exactly,
file by file. A new build that touches three files deploys three files —
not an image, not a layer, not a tarball. Re-deploying an unchanged ref
transfers zero bytes and is limited by ssh handshake time.

## Operational notes

- **Security** — there is no new protocol surface: transport security is
  your ssh. The remote side re-hashes everything and re-derives the
  manifest id, so a compromised local client still cannot poison the
  server's store with mislabeled content.
- **Testing without a server** — the transport is injectable:
  `MYCEL_SSH_CMD="env MYCEL_STORE=/tmp/remote bash -c"` runs the "remote"
  side locally against a separate store. `scripts/e2e.sh` uses this to
  exercise the full deploy path (first deploy / zero-delta re-deploy /
  incremental version bump) on every run.
- **Processes** — `--run` is deliberately minimal V1 process management:
  `nohup` + pidfile per ref (`~/.mycel/deploys/<ref>.pid`), logs next to
  it. `--restart` kills the recorded pid before starting. For anything
  more, put the `myc run` command in a systemd unit.
