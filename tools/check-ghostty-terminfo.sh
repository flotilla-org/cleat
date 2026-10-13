#!/usr/bin/env bash
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="$REPO_ROOT/.tools/ghostty-install"
SOURCE_DIR="$REPO_ROOT/.tools/ghostty-src"

# The prepare contract installs a compiled xterm-ghostty entry and its license.
test -s "$INSTALL_DIR/share/terminfo/x/xterm-ghostty"
cmp "$SOURCE_DIR/LICENSE" "$INSTALL_DIR/share/terminfo/LICENSE"
infocmp -A "$INSTALL_DIR/share/terminfo" xterm-ghostty > /dev/null

# Re-emitting from the same pin must produce byte-identical output.
# prepare has already verified/provisioned this Zig version.
version=$(awk '/^version = / {gsub(/"/, "", $3); print $3}' "$REPO_ROOT/tools/ghostty-toolchain.toml")
zig=$(command -v zig || true)
if [[ -z "$zig" ]] || [[ "$("$zig" version)" != "$version" ]]; then
  case "$(uname -s)" in
    Darwin) zig_os=macos ;;
    Linux) zig_os=linux ;;
    *) echo "Unsupported Zig host OS" >&2; exit 1 ;;
  esac
  case "$(uname -m)" in
    arm64|aarch64) zig_arch=aarch64 ;;
    x86_64|amd64) zig_arch=x86_64 ;;
    *) echo "Unsupported Zig host architecture" >&2; exit 1 ;;
  esac
  zig="$REPO_ROOT/.tools/zig-$zig_arch-$zig_os-$version/zig"
fi
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
(cd "$SOURCE_DIR" && "$zig" run src/emit-cleat-terminfo.zig) > "$temporary/ghostty.terminfo"
tic -x -o "$temporary/terminfo" "$temporary/ghostty.terminfo"
cmp "$INSTALL_DIR/share/terminfo/x/xterm-ghostty" "$temporary/terminfo/x/xterm-ghostty"
