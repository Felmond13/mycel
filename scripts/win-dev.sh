#!/usr/bin/env bash
# Helper for developing from the Windows side (files written over
# \\wsl.localhost get CRLF endings; PowerShell mangles inline $VAR).
#
#   win-dev.sh crlf      strip CRLF from all tracked source trees
#   win-dev.sh jscheck   syntax-check every frontend ES module with node
#   win-dev.sh smoke     serve a throwaway store and curl every asset route
#   win-dev.sh gate      test summary + release build
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

crlf() {
  find crates scripts docs -type f -not -path '*/vendor/*' -not -path '*/target/*' -print0 \
    | xargs -0 sed -i 's/\r$//'
  echo "CRLF stripped"
}

jscheck() {
  if ! command -v node >/dev/null; then
    echo "node not available in WSL - skip (validate from Windows)"
    return 0
  fi
  node --experimental-vm-modules scripts/jscheck.mjs
  node scripts/jsimport.mjs
  node scripts/jstest.mjs
}

smoke() {
  local work port=7899
  work="$(mktemp -d)"
  trap 'kill "$PID" 2>/dev/null; rm -rf "$work"' RETURN
  MYCEL_STORE="$work/store" ./target/release/myc ui --port "$port" &
  PID=$!
  for _ in $(seq 1 50); do
    curl -sf "http://127.0.0.1:$port/api/stats" >/dev/null 2>&1 && break
    sleep 0.1
  done
  local paths=(/ /style.css /vendor/vue.js /js/main.js)
  while IFS= read -r f; do
    paths+=("/js/${f#crates/myc-web/assets/js/}")
  done < <(find crates/myc-web/assets/js -name '*.js')
  for path in "${paths[@]}"; do
    local code
    code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port$path")"
    echo "$path -> $code"
    [ "$code" = "200" ] || return 1
  done
}

gate() {
  echo "== js (syntax + module graph + unit tests) =="
  jscheck
  echo "== clippy + fmt =="
  cargo clippy --all-targets -- -D warnings 2>&1 | tail -1
  cargo fmt --check && echo "fmt clean"
  echo "== tests =="
  cargo test 2>&1 | grep -E '^test result' | awk '{p+=$4; f+=$6} END {print p" passed, "f" failed"}'
  echo "== release build =="
  cargo build --release 2>&1 | tail -1
}

case "${1:-}" in
  crlf) crlf ;;
  jscheck) jscheck ;;
  smoke) smoke ;;
  gate) gate ;;
  *) echo "usage: $0 crlf|jscheck|smoke|gate"; exit 2 ;;
esac
