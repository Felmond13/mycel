#!/usr/bin/env bash
# Un run réel e2e pour hyperfine (bench/warm-pool/baseline.sh) : vérifie la
# sortie ET le code de retour — un run silencieusement cassé échoue la
# mesure (LOI N°1 : mesuré, pas inventé).
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
MYC=${MYC:-$(cd "$HERE/../.." && pwd)/target/release/myc}
IMG=${IMG:-alpine:3.20}
out=$("$MYC" run --engine kvm-microvm "$IMG" -- /bin/echo ok 2>/dev/null)
code=$?
if [ "$code" -ne 0 ] || [ "$out" != "ok" ]; then
  echo "run invalide (code=$code sortie='$out')" >&2
  exit 1
fi
exit 0
