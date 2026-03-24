#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${CLEAT_INSTALL_DIR:-$HOME/.local}"
BIN_DIR="$INSTALL_DIR/bin"
LIB_DIR="$INSTALL_DIR/lib"

# Build ghostty-vt if needed
"$REPO_ROOT/tools/prepare-ghostty-vt.sh"

GHOSTTY_PREFIX="$REPO_ROOT/.tools/ghostty-install"

# Build cleat with ghostty-vt
echo "Building cleat..."
(cd "$REPO_ROOT" && \
  CLEAT_GHOSTTY_PREFIX="$GHOSTTY_PREFIX" \
  cargo build -p cleat --locked --features ghostty-vt --release)

# Install binary and dylib
mkdir -p "$BIN_DIR" "$LIB_DIR"

cp "$REPO_ROOT/target/release/cleat" "$BIN_DIR/cleat"

# Copy ghostty-vt dylib
case "$(uname -s)" in
  Darwin) DYLIB="libghostty-vt.dylib" ;;
  Linux)  DYLIB="libghostty-vt.so" ;;
  *)      echo "Unsupported OS" >&2; exit 1 ;;
esac

cp "$GHOSTTY_PREFIX/lib/$DYLIB" "$LIB_DIR/$DYLIB"

# On macOS, add rpath so the binary finds the dylib in ../lib relative to itself
if [[ "$(uname -s)" == "Darwin" ]]; then
  install_name_tool -add_rpath "@executable_path/../lib" "$BIN_DIR/cleat" 2>/dev/null || true
fi

echo "Installed cleat to $BIN_DIR/cleat"
echo "Installed $DYLIB to $LIB_DIR/$DYLIB"
