#!/bin/sh
# Mycel binary installer (Linux x86_64, WSL2 included).
#
#   curl -fsSL https://raw.githubusercontent.com/Felmond13/mycel/main/scripts/install.sh | sh
#
# Downloads the prebuilt `myc-linux-x86_64` binary from the latest GitHub
# release into ~/.local/bin (/usr/local/bin when run as root). When /dev/kvm
# is present it offers — or with --with-vm installs without asking — the
# KVM microVM guest assets (vmlinux-vmfs + init-vm.cpio) into
# /usr/local/share/mycel/. Finishes with `myc doctor`.
#
# Options:
#   --version vX.Y.Z   install a specific release tag (default: latest)
#   --with-vm          install the microVM guest assets without prompting
#   -h | --help        this help
#
# Environment overrides (for testing / offline mirrors):
#   MYCEL_INSTALL_API    releases API root
#                        (default https://api.github.com/repos/Felmond13/mycel)
#   MYCEL_INSTALL_BASE   download base URL for release assets
#                        (default https://github.com/Felmond13/mycel/releases/download)
#   MYCEL_ASSET_DIR      destination for guest assets (default /usr/local/share/mycel)
#
# The client never compiles anything: sources are public (BUSL-1.1), the
# distribution is prebuilt binaries.

set -eu

REPO=Felmond13/mycel
API=${MYCEL_INSTALL_API:-https://api.github.com/repos/$REPO}
DLBASE=${MYCEL_INSTALL_BASE:-https://github.com/$REPO/releases/download}
ASSET_DIR=${MYCEL_ASSET_DIR:-/usr/local/share/mycel}
VERSION=latest
WITH_VM=no

note() { echo "==> $*"; }
die()  { echo "install: error: $*" >&2; exit 1; }

usage() {
    sed -n '2,30p' "$0" 2>/dev/null || echo "see the header of scripts/install.sh"
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || die "--version needs a value (e.g. --version v0.2.0)"
            VERSION=$2; shift 2 ;;
        --version=*) VERSION=${1#--version=}; shift ;;
        --with-vm) WITH_VM=yes; shift ;;
        -h|--help) usage ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
done

# --- platform gate ---------------------------------------------------------
[ "$(uname -s)" = "Linux" ] || die "prebuilt binaries are Linux only; on Windows use myc-windows-x86_64.exe from the releases page"
[ "$(uname -m)" = "x86_64" ] || die "no prebuilt binary for $(uname -m) yet - build from source: cargo build --release -p myc-cli"
if grep -qi microsoft /proc/version 2>/dev/null; then
    note "WSL2 detected - the Linux x86_64 binary is the supported path here"
fi

command -v curl >/dev/null 2>&1 || die "curl is required (apt-get install curl)"

TMP=$(mktemp -d) || die "cannot create a temp dir"
trap 'rm -rf "$TMP"' EXIT

fetch() { # url dest  (follows redirects; GitHub assets redirect to S3)
    curl -fsSL --connect-timeout 15 --max-time 600 "$1" -o "$2" || return 1
}
fetch_asset() { # url dest min_bytes label
    fetch "$1" "$2" || die "download failed: $1 (offline? rate-limited? tag $VERSION missing the asset?)"
    size=$(wc -c < "$2" | tr -d ' ')
    [ "$size" -ge "$3" ] || die "$4: got only $size bytes (expected >= $3) - truncated download?"
}

# --- resolve the version ---------------------------------------------------
if [ "$VERSION" = latest ]; then
    note "resolving the latest release from $API"
    json=$(curl -fsSL "$API/releases/latest") || die "cannot reach the releases API at $API"
    VERSION=$(printf '%s' "$json" | grep -oE '"tag_name"[[:space:]]*:[[:space:]]*"[^"]+"' | head -1 | sed 's/^.*: *"\([^"]*\)"$/\1/')
    [ -n "$VERSION" ] && [ "$VERSION" != latest ] || die "could not parse tag_name from the API response"
fi
note "installing mycel $VERSION"

# --- 1. the binary ---------------------------------------------------------
if [ "$(id -u)" = "0" ]; then
    DEST_DIR=/usr/local/bin
else
    DEST_DIR=$HOME/.local/bin
fi

note "downloading myc-linux-x86_64"
fetch_asset "$DLBASE/$VERSION/myc-linux-x86_64" "$TMP/myc" 1000000 "myc-linux-x86_64"
chmod 755 "$TMP/myc"

mkdir -p "$DEST_DIR"
mv -f "$TMP/myc" "$DEST_DIR/.myc.incoming"
mv -f "$DEST_DIR/.myc.incoming" "$DEST_DIR/myc"   # atomic-ish replace of a possibly running binary
chmod 755 "$DEST_DIR/myc"

case ":$PATH:" in
    *":$DEST_DIR:"*) ;;
    *) echo "NOTE: $DEST_DIR is not on your PATH. Add it with:"
       echo "  echo 'export PATH=\"$DEST_DIR:\$PATH\"' >> ~/.profile && . ~/.profile" ;;
esac
note "installed $("$DEST_DIR/myc" --version 2>/dev/null || echo myc) -> $DEST_DIR/myc"

# --- 2. microVM guest assets (optional) ------------------------------------
wants_vm() {
    [ "$WITH_VM" = yes ] && return 0
    [ -e /dev/kvm ] || return 1
    # Interactive offer only when a human is watching AND can answer
    # (curl | sh: stdin is the script itself, so the prompt must go through
    # /dev/tty; piped/CI runs degrade to a skip with the hint below).
    if [ -t 1 ] && [ -r /dev/tty ]; then
        printf '==> /dev/kvm detected. Also install the KVM microVM guest assets\n    (vmlinux-vmfs + init-vm.cpio, ~7 MB) into %s? [y/N] ' "$ASSET_DIR"
        if read -r ans </dev/tty 2>/dev/null; then
            case "$ans" in y|Y|yes|YES|Yes) return 0 ;; esac
        fi
    fi
    return 1
}

install_vm_assets() {
    note "downloading microVM guest assets (kernel + initramfs)"
    fetch_asset "$DLBASE/$VERSION/vmlinux-vmfs" "$TMP/vmlinux-vmfs" 1000000 "vmlinux-vmfs"
    fetch_asset "$DLBASE/$VERSION/init-vm.cpio" "$TMP/init-vm.cpio" 65536 "init-vm.cpio"
    # Verify sha256 sidecars when the release ships them (skipped silently
    # on releases without sidecars or machines without sha256sum).
    if command -v sha256sum >/dev/null 2>&1; then
        for a in vmlinux-vmfs init-vm.cpio; do
            if fetch "$DLBASE/$VERSION/$a.sha256" "$TMP/$a.sha256"; then
                (cd "$TMP" && sha256sum -c "$a.sha256" >/dev/null) \
                    || die "sha256 mismatch for $a - corrupt download, please retry"
                note "sha256 ok: $a"
            fi
        done
    fi
    SUDO=
    if [ "$(id -u)" != 0 ] && [ ! -w "$(dirname "$ASSET_DIR")" ]; then
        command -v sudo >/dev/null 2>&1 || die "installing into $ASSET_DIR needs root - re-run with sudo or set MYCEL_ASSET_DIR"
        SUDO=sudo
    fi
    $SUDO mkdir -p "$ASSET_DIR"
    for a in vmlinux-vmfs init-vm.cpio; do
        $SUDO mv -f "$TMP/$a" "$ASSET_DIR/$a"
    done
    note "guest assets installed: $ASSET_DIR/vmlinux-vmfs, $ASSET_DIR/init-vm.cpio"
}

if wants_vm; then
    install_vm_assets
elif [ "$WITH_VM" != yes ] && [ ! -e /dev/kvm ]; then
    note "no /dev/kvm - skipping the microVM guest assets (install.sh --with-vm to force)"
fi

# --- 3. health check -------------------------------------------------------
echo
note "running 'myc doctor'"
if ! "$DEST_DIR/myc" doctor; then
    echo "(doctor reported the issues above - they come with suggested fixes)"
fi
echo
note "done. Try: myc run docker.io/library/alpine:3.20 -- /bin/echo hello"
