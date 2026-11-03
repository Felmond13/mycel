#!/usr/bin/env bash
# Image Docker benchhttp:microvm — FROM scratch + le MÊME binaire musl que
# la microVM. Aucun pull réseau (base vide).
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
BIN="$HERE/assets/benchhttp"

[ -x "$BIN" ] || { echo "binaire manquant : $BIN (lancer mkinitramfs.sh d'abord)" >&2; exit 1; }

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp "$BIN" "$STAGE/benchhttp"
chmod 755 "$STAGE/benchhttp"
cat > "$STAGE/Dockerfile" <<'EOF'
FROM scratch
COPY benchhttp /benchhttp
ENTRYPOINT ["/benchhttp"]
EOF

docker build -q -t benchhttp:microvm "$STAGE"
docker image inspect benchhttp:microvm --format 'image benchhttp:microvm prete — {{.Size}} octets'
sha256sum "$BIN"
