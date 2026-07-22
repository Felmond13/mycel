# Mycel in your CI

CI is where container overhead hurts the most: every job starts from a blank
machine, so every job re-downloads the same images. Docker caches **layers**
— change one file and the whole layer is invalidated and re-pulled. Mycel
caches **files**: restore the store between runs and a pull downloads only
the files that actually changed.

## The numbers

Take a typical integration-test job using `postgres:16` and `redis:7`,
plus an app image that changes on every commit:

| | Docker (`docker pull`) | Mycel (cached store) |
|---|---|---|
| First run | ~350 MB | ~350 MB (same: everything is new) |
| Next run, nothing changed | ~350 MB again* | **0 bytes** — every hash already local |
| Next run, app image rebuilt (1 file changed) | full changed layers, often 50–200 MB | **just the changed files**, typically < 1 MB |

\* GitHub-hosted runners are ephemeral: without extra machinery (registry
mirrors, docker/build-push-action cache exports…) Docker starts cold every
time. Mycel needs nothing but `actions/cache`, because the store is a plain
directory of content-addressed files.

The mechanism is not a heuristic: every file in the store is named by its
BLAKE3 hash, so `myc pull` compares manifests and fetches exactly the blobs
it does not have. A stale cache is never *wrong* — at worst it holds extra
files (`myc gc` trims it).

## Setup in one step

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      # Installs myc, exports MYCEL_STORE, and wires the store into
      # actions/cache. Warm runs start with every known file already local.
      - uses: Felmond13/mycel/action@v0.2.0

      - run: myc pull postgres:16   # warm cache: verifies, downloads ~0 B
      - run: myc run postgres:16 -- postgres --version
```

The action's inputs (`version`, `store-path`, `cache`, `cache-key-prefix`,
`token`) are documented in [`action/README.md`](../action/README.md), along
with a manual `actions/cache` wiring if you prefer explicit control.

## Why this works on GitHub runners

- Mycel is rootless: it uses unprivileged user namespaces, which
  `ubuntu-latest` runners allow. No daemon to start, no `sudo`.
- The binary is a single static executable, installed in under a second.
- The store is a plain directory — `actions/cache` handles it natively.

## Patterns

**Pin by digest, not tag.** Manifest ids are BLAKE3 hashes of content;
`myc pull` + `myc verify` give you a cryptographic guarantee that CI ran
against the exact bytes you audited, something a mutable Docker tag cannot.

**Offline-ready guarantee.** `myc pull` proves every referenced blob is
local. A job that passed `myc pull` cannot fail later because a registry
went down mid-run.

**Trim the cache when it grows.** Stores only grow when new content shows
up. If your app image churns daily, add a cleanup step:

```yaml
      - run: myc rm old-tag && myc gc   # optional: keep the cache lean
```

**Air-gapped runners.** `myc export` / `myc import` move environments as a
single archive — useful for self-hosted runners with no registry access.
