#!/usr/bin/env bash
# Un run réel de bout en bout pour hyperfine (bench/snapshot-accel/run.sh) :
# vérifie la sortie ET le code — un run cassé fait échouer la mesure.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
MYC=${MYC:-$(cd "$HERE/../.." && pwd)/target/debug/myc}
IMG=${IMG:-alpine:3.20}
out=$(MYCEL_SNAPSHOT_DIR="$HERE/.snapcache" "$MYC" run --engine kvm-microvm "$IMG" -- /bin/echo ok 2>/dev/null)
code=$?
if [ "$code" -ne 0 ] || [ "$out" != "ok" ]; then
  echo "run invalide (code=$code sortie='$out')" >&2
  exit 1
fi
