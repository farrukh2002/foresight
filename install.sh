#!/bin/bash
set -euo pipefail

OWNER="farrukh2002"
REPO="foresight"
BIN_NAME="foresight"

BIN_DIR="$HOME/.local/bin"
SHARE_DIR="$HOME/.local/share/foresight"
CONFIG_DIR="$HOME/.config/foresight"

VERSION="${FORESIGHT_VERSION:-}"
CHANNEL="${FORESIGHT_CHANNEL:-stable}"

case "$CHANNEL" in
    stable|beta|dev) ;;
    *) echo "error: invalid FORESIGHT_CHANNEL '$CHANNEL' (expected stable, beta, or dev)" >&2; exit 1 ;;
esac

detect_asset() {
    local os arch
    os=$(uname -s)
    arch=$(uname -m)
    if [ "$os" != "Linux" ]; then
        echo "error: prebuilt binaries are Linux-only (detected: $os)" >&2
        exit 1
    fi
    case "$arch" in
        x86_64) echo "foresight-linux-x86_64.tar.gz" ;;
        aarch64|arm64) echo "foresight-linux-aarch64.tar.gz" ;;
        *) echo "error: unsupported architecture: $arch" >&2; exit 1 ;;
    esac
}

ASSET=$(detect_asset)

if [ -z "$VERSION" ]; then
    VERSION=$(curl -fsSL "https://github.com/$OWNER/$REPO/releases/download/$CHANNEL/VERSION")
fi

echo "Installing foresight $VERSION ($ASSET, $CHANNEL channel)..."

BASE="https://github.com/$OWNER/$REPO/releases/download/$VERSION"
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL -o "$TMP_DIR/$ASSET" "$BASE/$ASSET"
curl -fsSL -o "$TMP_DIR/SHA256SUMS" "$BASE/SHA256SUMS"

EXPECTED=$(grep " $ASSET\$" "$TMP_DIR/SHA256SUMS" | cut -d' ' -f1)
ACTUAL=$(sha256sum "$TMP_DIR/$ASSET" | cut -d' ' -f1)
if [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "error: checksum mismatch for $ASSET" >&2
    exit 1
fi

tar -xzf "$TMP_DIR/$ASSET" -C "$TMP_DIR"

mkdir -p "$BIN_DIR" "$SHARE_DIR" "$CONFIG_DIR"
install -m 755 "$TMP_DIR/$BIN_NAME" "$BIN_DIR/$BIN_NAME"
curl -fsSL -o "$SHARE_DIR/foresight.sh" "https://raw.githubusercontent.com/$OWNER/$REPO/main/foresight.sh"

SOURCE_LINE="source \"$SHARE_DIR/foresight.sh\""
BASHRC="$HOME/.bashrc"
if ! grep -qF "$SOURCE_LINE" "$BASHRC" 2>/dev/null; then
    printf '\n%s\n' "$SOURCE_LINE" >> "$BASHRC"
    echo "Added source line to $BASHRC"
fi

"$BIN_DIR/$BIN_NAME" ensure-daemon >/dev/null 2>&1 || true

if [ -t 0 ] && [ -t 1 ]; then
    read -r -p "Enable silent background auto-updates? [y/N] " REPLY
    if [[ "$REPLY" =~ ^[Yy]$ ]]; then
        "$BIN_DIR/$BIN_NAME" update --enable-silent
    else
        echo "Silent auto-update left off. Enable later with: $BIN_NAME update --enable-silent"
    fi
else
    echo "Silent auto-update left off (non-interactive install). Enable with: $BIN_NAME update --enable-silent"
fi

echo "Installed. Start a new shell, or run: $SOURCE_LINE"
