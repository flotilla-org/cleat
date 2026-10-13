#!/usr/bin/env bash
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="$REPO_ROOT/.tools/ghostty-install"
SOURCE_DIR="$REPO_ROOT/.tools/ghostty-src"

# The prepare contract installs a compiled xterm-ghostty entry and its license.
test -s "$INSTALL_DIR/share/terminfo/x/xterm-ghostty"
cmp "$SOURCE_DIR/LICENSE" "$INSTALL_DIR/share/terminfo/LICENSE"
# Ghostty's pinned definition advertises 256 colors, not merely a valid entry.
description=$(infocmp -A "$INSTALL_DIR/share/terminfo" xterm-ghostty)
grep -Eq 'colors#(256|0x100),' <<< "$description"

# Re-emitting from the same pin must produce byte-identical output.
# prepare has already verified/provisioned this Zig version.
version=$(awk '/^version = / {gsub(/"/, "", $3); print $3}' "$REPO_ROOT/tools/ghostty-toolchain.toml")
zig=$(cat "$SOURCE_DIR/.cleat-zig-path")
[[ "$("$zig" version)" == "$version" ]]
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
(cd "$SOURCE_DIR" && "$zig" run src/emit-cleat-terminfo.zig) > "$temporary/ghostty.terminfo"
tic -x -o "$temporary/terminfo" "$temporary/ghostty.terminfo"
entry="$temporary/terminfo/x/xterm-ghostty"
[[ -f "$entry" ]] || entry="$temporary/terminfo/78/xterm-ghostty"
cmp "$INSTALL_DIR/share/terminfo/x/xterm-ghostty" "$entry"
