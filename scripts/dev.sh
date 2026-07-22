#!/usr/bin/env bash
# Development helper used both by contributors and by CI.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

case "${1:-build}" in
  build)   cargo build ;;
  release) cargo build --release ;;
  test)    cargo test ;;
  check)   cargo clippy --all-targets -- -D warnings && cargo fmt --check ;;
  *) echo "usage: $0 [build|release|test|check]"; exit 2 ;;
esac
