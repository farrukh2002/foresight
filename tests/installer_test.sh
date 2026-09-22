#!/bin/bash
set -euo pipefail

# Focused independent installer tests for install.sh
# Tests run entirely offline with mock curl/binaries and isolated HOME directories.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALL_SH="$REPO_DIR/install.sh"

TEST_TMP=$(mktemp -d "/tmp/foresight_installer_test_XXXXXX")
trap 'rm -rf "$TEST_TMP"' EXIT

MOCK_BIN_DIR="$TEST_TMP/mock_bin"
mkdir -p "$MOCK_BIN_DIR"

# Create a mock foresight binary that supports "init --quiet" and "ensure-daemon"
cat << 'EOF' > "$MOCK_BIN_DIR/mock_foresight"
#!/bin/bash
case "${1:-}" in
    init)
        # Materialize integrations directory like foresight init
        data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
        mkdir -p "$data_dir/foresight"
        cat > "$data_dir/foresight/foresight.sh" << 'INNER'
# foresight.sh integration marker
FORESIGHT_INTEGRATION_SOURCED=1
INNER
        cat > "$data_dir/foresight/foresight.zsh" << 'INNER'
# foresight.zsh integration marker
FORESIGHT_INTEGRATION_SOURCED=1
INNER
        # Notice: real foresight init creates blesh layer dir
        mkdir -p "$data_dir/blesh/local/integration"
        touch "$data_dir/blesh/local/integration/foresight.bash"
        exit 0
        ;;
    ensure-daemon)
        exit 0
        ;;
    update)
        exit 0
        ;;
    *)
        exit 0
        ;;
esac
EOF
chmod +x "$MOCK_BIN_DIR/mock_foresight"

# Package mock foresight into an asset tarball
ASSET_NAME="foresight-linux-x86_64.tar.gz"
TAR_DIR="$TEST_TMP/tar_staging"
mkdir -p "$TAR_DIR"
cp "$MOCK_BIN_DIR/mock_foresight" "$TAR_DIR/foresight"
tar -czf "$TEST_TMP/$ASSET_NAME" -C "$TAR_DIR" foresight
SHA256_VAL=$(sha256sum "$TEST_TMP/$ASSET_NAME" | cut -d' ' -f1)
echo "$SHA256_VAL  $ASSET_NAME" > "$TEST_TMP/SHA256SUMS"

# Create mock curl that intercepts downloads and returns our test artifacts
cat << EOF > "$MOCK_BIN_DIR/curl"
#!/bin/bash
out_file=""
while [ \$# -gt 0 ]; do
    case "\$1" in
        -o)
            out_file="\$2"
            shift 2
            ;;
        *)
            url="\$1"
            shift
            ;;
    esac
done

if [[ "\$url" == */VERSION ]]; then
    echo "0.6.0-stable"
elif [[ "\$url" == */SHA256SUMS ]]; then
    cp "$TEST_TMP/SHA256SUMS" "\$out_file"
elif [[ "\$url" == *"$ASSET_NAME" ]]; then
    cp "$TEST_TMP/$ASSET_NAME" "\$out_file"
else
    echo "mock curl unhandled: \$url" >&2
    exit 1
fi
EOF
chmod +x "$MOCK_BIN_DIR/curl"

# Create mock zsh binary in PATH so command -v zsh succeeds even on machines without system zsh
touch "$MOCK_BIN_DIR/zsh"
chmod +x "$MOCK_BIN_DIR/zsh"

# Put mock curl in PATH
ORIG_PATH="$PATH"
export PATH="$MOCK_BIN_DIR:$ORIG_PATH"

echo "=== Test 1: Clean HOME with zsh, no initial rc files ==="
TEST_HOME_1="$TEST_TMP/home1"
mkdir -p "$TEST_HOME_1"

# Run install.sh with clean HOME
HOME="$TEST_HOME_1" bash "$INSTALL_SH" </dev/null

# Verify .zshrc is created
ZSHRC_1="$TEST_HOME_1/.zshrc"
BASHRC_1="$TEST_HOME_1/.bashrc"
if [ ! -f "$ZSHRC_1" ]; then
    echo "FAIL: .zshrc was not created in clean home" >&2
    exit 1
fi

# Behavioral check: source .zshrc under a clean PATH and observe executable
# discovery (command -v foresight finds the installed binary) and that the
# integration script was actually sourced (marker variable set).
sourced_env=$(env -i HOME="$TEST_HOME_1" PATH="/usr/bin:/bin" bash -c 'source "$HOME/.zshrc" >/dev/null 2>&1; echo "FOUND=$(command -v foresight)"; echo "MARKER=${FORESIGHT_INTEGRATION_SOURCED:-}"')
echo "$sourced_env" | grep -q "FOUND=$TEST_HOME_1/.local/bin/foresight" || { echo "FAIL: zsh rc did not discover foresight on clean PATH (got: $sourced_env)" >&2; exit 1; }
echo "$sourced_env" | grep -q "MARKER=1" || { echo "FAIL: foresight.zsh integration not sourced (got: $sourced_env)" >&2; exit 1; }

# Behavioral check: same for .bashrc
sourced_env_bash=$(env -i HOME="$TEST_HOME_1" PATH="/usr/bin:/bin" bash -c 'source "$HOME/.bashrc" >/dev/null 2>&1; echo "FOUND=$(command -v foresight)"; echo "MARKER=${FORESIGHT_INTEGRATION_SOURCED:-}"')
echo "$sourced_env_bash" | grep -q "FOUND=$TEST_HOME_1/.local/bin/foresight" || { echo "FAIL: bash rc did not discover foresight on clean PATH (got: $sourced_env_bash)" >&2; exit 1; }
echo "$sourced_env_bash" | grep -q "MARKER=1" || { echo "FAIL: foresight.sh integration not sourced (got: $sourced_env_bash)" >&2; exit 1; }

# Verify ble.sh config was NOT invented
if [ -f "$TEST_HOME_1/.blerc" ] || [ -f "$TEST_HOME_1/.config/blesh/init.sh" ]; then
    echo "FAIL: ble.sh config was invented on absent ble.sh" >&2
    exit 1
fi
echo "Test 1 PASSED."

echo "=== Test 2: Idempotence: No duplicate PATH or source lines on second run ==="
HOME="$TEST_HOME_1" bash "$INSTALL_SH" </dev/null

PATH_COUNT=$(grep -cxF 'case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) export PATH="$HOME/.local/bin:$PATH" ;; esac' "$BASHRC_1" || true)
if [ "$PATH_COUNT" -ne 1 ]; then
    echo "FAIL: duplicate PATH lines in .bashrc (count: $PATH_COUNT)" >&2
    exit 1
fi

ZSH_SRC_COUNT=$(grep -cxF 'source "$HOME/.local/share/foresight/foresight.zsh"' "$ZSHRC_1" || true)
if [ "$ZSH_SRC_COUNT" -ne 1 ]; then
    echo "FAIL: duplicate source lines in .zshrc (count: $ZSH_SRC_COUNT)" >&2
    exit 1
fi

BASH_SRC_COUNT=$(grep -cxF 'source "$HOME/.local/share/foresight/foresight.sh"' "$BASHRC_1" || true)
if [ "$BASH_SRC_COUNT" -ne 1 ]; then
    echo "FAIL: duplicate source lines in .bashrc (count: $BASH_SRC_COUNT)" >&2
    exit 1
fi
if [ -f "$TEST_HOME_1/.blerc" ] || [ -f "$TEST_HOME_1/.config/blesh/init.sh" ]; then
    echo "FAIL: ble.sh config was invented on second run" >&2
    exit 1
fi
echo "Test 2 PASSED."

echo "=== Test 3: Respect ZDOTDIR ==="
TEST_HOME_3="$TEST_TMP/home3"
CUSTOM_ZDOTDIR="$TEST_TMP/custom_zdotdir"
mkdir -p "$TEST_HOME_3" "$CUSTOM_ZDOTDIR"

HOME="$TEST_HOME_3" ZDOTDIR="$CUSTOM_ZDOTDIR" bash "$INSTALL_SH" </dev/null

if [ -f "$TEST_HOME_3/.zshrc" ]; then
    echo "FAIL: .zshrc should not be created in HOME when ZDOTDIR is set" >&2
    exit 1
fi
if [ ! -f "$CUSTOM_ZDOTDIR/.zshrc" ]; then
    echo "FAIL: .zshrc not created in ZDOTDIR" >&2
    exit 1
fi
# Behavioral: sourcing ZDOTDIR/.zshrc with HOME exported discovers binary
sourced_env_zdot=$(env -i HOME="$TEST_HOME_3" PATH="/usr/bin:/bin" bash -c 'source "'"$CUSTOM_ZDOTDIR"'/.zshrc" >/dev/null 2>&1; echo "FOUND=$(command -v foresight)"')
echo "$sourced_env_zdot" | grep -q "FOUND=$TEST_HOME_3/.local/bin/foresight" || { echo "FAIL: ZDOTDIR rc did not discover foresight (got: $sourced_env_zdot)" >&2; exit 1; }
echo "Test 3 PASSED."

echo "=== Test 4: Real ble.sh present is recognized and configured ==="
TEST_HOME_4="$TEST_TMP/home4"
mkdir -p "$TEST_HOME_4"
# Pre-create .blerc indicating ble.sh is used
touch "$TEST_HOME_4/.blerc"

HOME="$TEST_HOME_4" bash "$INSTALL_SH" </dev/null

grep -qF "ble-import integration/foresight" "$TEST_HOME_4/.blerc" || { echo "FAIL: ble.sh not registered when .blerc was present" >&2; exit 1; }
echo "Test 4 PASSED."

echo "=== Test 5: HOME containing spaces and shell punctuation ==="
TEST_HOME_5="$TEST_TMP/home5 with space\$and'quote"
mkdir -p "$TEST_HOME_5"

HOME="$TEST_HOME_5" bash "$INSTALL_SH" </dev/null

# Behavioral: rc files with weird HOME must still discover the binary on clean PATH
sourced_env_5=$(env -i HOME="$TEST_HOME_5" PATH="/usr/bin:/bin" bash -c 'source "$HOME/.bashrc" >/dev/null 2>&1; echo "FOUND=$(command -v foresight)"; echo "MARKER=${FORESIGHT_INTEGRATION_SOURCED:-}"')
echo "$sourced_env_5" | grep -q "FOUND=$TEST_HOME_5/.local/bin/foresight" || { echo "FAIL: weird-HOME bash rc did not discover foresight (got: $sourced_env_5)" >&2; exit 1; }
echo "$sourced_env_5" | grep -q "MARKER=1" || { echo "FAIL: weird-HOME integration not sourced (got: $sourced_env_5)" >&2; exit 1; }

sourced_env_5z=$(env -i HOME="$TEST_HOME_5" PATH="/usr/bin:/bin" bash -c 'source "$HOME/.zshrc" >/dev/null 2>&1; echo "FOUND=$(command -v foresight)"; echo "MARKER=${FORESIGHT_INTEGRATION_SOURCED:-}"')
echo "$sourced_env_5z" | grep -q "FOUND=$TEST_HOME_5/.local/bin/foresight" || { echo "FAIL: weird-HOME zsh rc did not discover foresight (got: $sourced_env_5z)" >&2; exit 1; }
echo "$sourced_env_5z" | grep -q "MARKER=1" || { echo "FAIL: weird-HOME zsh integration not sourced (got: $sourced_env_5z)" >&2; exit 1; }

# Idempotence with weird HOME
HOME="$TEST_HOME_5" bash "$INSTALL_SH" </dev/null
BASHRC_5="$TEST_HOME_5/.bashrc"
PATH_COUNT_5=$(grep -cxF 'case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) export PATH="$HOME/.local/bin:$PATH" ;; esac' "$BASHRC_5" || true)
if [ "$PATH_COUNT_5" -ne 1 ]; then
    echo "FAIL: duplicate PATH lines in weird-HOME .bashrc (count: $PATH_COUNT_5)" >&2
    exit 1
fi
echo "Test 5 PASSED."

echo "All installer tests PASSED."
