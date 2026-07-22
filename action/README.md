# setup-mycel

Install [Mycel](https://github.com/Felmond13/mycel) (`myc`) in a GitHub
Actions job and persist its content-addressed store between runs.

Why bother? Docker pulls re-download **layers**: change one file in an image
and CI fetches the whole layer again, every job, every day. Mycel stores
**files**, each exactly once — restore the store from cache and a pull only
fetches the files that actually changed since the last run. Warm-cache pulls
routinely move a few hundred KB instead of hundreds of MB.

## Usage

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: Felmond13/mycel/action@v0.2.0   # or @main

      - run: myc run alpine:3.20 -- echo hello from CI
```

That is the whole setup. The action:

1. downloads the `myc` release binary (input `version`, default `latest`)
   and puts it on the `PATH`;
2. exports `MYCEL_STORE` (input `store-path`, default `~/.mycel`);
3. wires the store into `actions/cache` (disable with `cache: false`) so the
   next run starts with every previously seen file already local.

## Inputs

| Input | Default | Description |
|---|---|---|
| `version` | `latest` | Release tag to install (e.g. `v0.2.0`). |
| `store-path` | `~/.mycel` | Store location; exported as `MYCEL_STORE`. |
| `cache` | `true` | Automatically cache the store between runs. |
| `cache-key-prefix` | `mycel-store` | Change to force a cold store. |
| `token` | `github.token` | Token used to download the release asset. |

## Outputs

| Output | Description |
|---|---|
| `store-path` | Resolved absolute store path. |
| `version` | Output of `myc --version`. |

## Before / after

A typical integration-test job that needs postgres, redis and an app image:

**Before (Docker):** every run pulls the same ~350 MB of layers from the
registry. Change one config file in the app image and its whole layer is
re-downloaded too.

```yaml
steps:
  - uses: actions/checkout@v4
  - run: docker pull postgres:16 && docker pull redis:7   # ~350 MB, every run
  - run: docker run -d postgres:16 && docker run -d redis:7
  - run: ./run-tests.sh
```

**After (Mycel):** the first run ingests everything once; every following
run restores the store from cache in seconds and pulls only changed files.

```yaml
steps:
  - uses: actions/checkout@v4
  - uses: Felmond13/mycel/action@v0.2.0

  - run: |
      myc pull postgres:16      # warm cache: verifies hashes, downloads ~0 B
      myc pull redis:7
  - run: ./run-tests.sh         # myc run ... inside
```

If you prefer to manage the cache yourself, set `cache: false` and wire it
manually:

```yaml
  - uses: Felmond13/mycel/action@v0.2.0
    with:
      cache: false
      store-path: /tmp/mycel-store

  - uses: actions/cache@v4
    with:
      path: /tmp/mycel-store
      key: my-store-${{ runner.os }}-${{ github.run_id }}
      restore-keys: my-store-${{ runner.os }}-
```

More background and numbers: [docs/ci.md](../docs/ci.md).

## Notes

- Linux x86_64 runners only (`ubuntu-latest` works out of the box; Mycel is
  rootless and uses unprivileged user namespaces, which GitHub runners allow).
- The store is content-addressed, so a stale cache is never wrong — at worst
  it contains extra files. Run `myc gc` in a job if you want to trim it.
