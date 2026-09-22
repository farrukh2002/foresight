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
    VERSION=$(curl -fsSL "https://github.com/$OWNER/$REPO/releases/download/$CHANNEL/VERSION" | tr -d '\r\n')
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

# Capture actual ble.sh presence BEFORE init materializes its integration directory
HAS_BLESH=false
BASHRC="$HOME/.bashrc"
if grep -q 'ble\.sh' "$BASHRC" 2>/dev/null \
    || [ -f "$HOME/.blerc" ] \
    || [ -f "${XDG_CONFIG_HOME:-$HOME/.config}/blesh/init.sh" ] \
    || [ -f "${XDG_DATA_HOME:-$HOME/.local/share}/blesh/ble.sh" ] \
    || command -v ble.sh >/dev/null 2>&1; then
    HAS_BLESH=true
fi

"$BIN_DIR/$BIN_NAME" init --quiet

# Canonical rc statements use literal runtime $HOME so any HOME content
# (spaces, $, quotes, backticks) survives round-trip through the rc file.
PATH_LINE='case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) export PATH="$HOME/.local/bin:$PATH" ;; esac'
SOURCE_LINE='source "$HOME/.local/share/foresight/foresight.sh"'
ZSH_SOURCE_LINE='source "$HOME/.local/share/foresight/foresight.zsh"'

mkdir -p "$(dirname "$BASHRC")"
if ! grep -qxF "$PATH_LINE" "$BASHRC" 2>/dev/null; then
    printf '\n%s\n' "$PATH_LINE" >> "$BASHRC"
    echo "Added PATH entry to $BASHRC"
fi
if ! grep -qxF "$SOURCE_LINE" "$BASHRC" 2>/dev/null; then
    printf '\n%s\n' "$SOURCE_LINE" >> "$BASHRC"
    echo "Added source line to $BASHRC"
fi

if [ "$HAS_BLESH" = "true" ]; then
    BLESH_RC=""
    [ -f "$HOME/.blerc" ] && BLESH_RC="$HOME/.blerc"
    [ -z "$BLESH_RC" ] && BLESH_RC="${XDG_CONFIG_HOME:-$HOME/.config}/blesh/init.sh"
    BLESH_LINE="ble-import integration/foresight"
    if ! grep -qF "$BLESH_LINE" "$BLESH_RC" 2>/dev/null; then
        mkdir -p "$(dirname "$BLESH_RC")"
        printf '\n# added by foresight\n%s\n' "$BLESH_LINE" >> "$BLESH_RC"
        echo "ble.sh: registered in $BLESH_RC"
    fi
fi

if command -v zsh >/dev/null 2>&1; then
    ZSH_RC="${ZDOTDIR:-$HOME}/.zshrc"
    mkdir -p "$(dirname "$ZSH_RC")"
    if ! grep -qxF "$PATH_LINE" "$ZSH_RC" 2>/dev/null; then
        printf '\n%s\n' "$PATH_LINE" >> "$ZSH_RC"
        echo "zsh: added PATH entry to $ZSH_RC"
    fi
    if ! grep -qxF "$ZSH_SOURCE_LINE" "$ZSH_RC" 2>/dev/null; then
        printf '\n# added by foresight\n%s\n' "$ZSH_SOURCE_LINE" >> "$ZSH_RC"
        echo "zsh: registered in $ZSH_RC"
    fi
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
