#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLCHAIN_FILE="$REPO_ROOT/tools/ghostty-toolchain.toml"
SOURCE_DIR="$REPO_ROOT/.tools/ghostty-src"
INSTALL_DIR="$REPO_ROOT/.tools/ghostty-install"

toml_value() {
  local section="$1"
  local key="$2"
  local file="$3"
  awk -v section="$section" -v key="$key" '
    function trim(value) {
      sub(/^[[:space:]]+/, "", value)
      sub(/[[:space:]]+$/, "", value)
      return value
    }

    /^[[:space:]]*\[/ {
      current = $0
      sub(/^[[:space:]]*\[/, "", current)
      sub(/\][[:space:]]*$/, "", current)
      next
    }

    current == section {
      line = $0
      sub(/#.*/, "", line)
      if (line ~ "^[[:space:]]*" key "[[:space:]]*=") {
        sub("^[[:space:]]*" key "[[:space:]]*=[[:space:]]*", "", line)
        gsub(/^"/, "", line)
        gsub(/"$/, "", line)
        print trim(line)
        exit 0
      }
    }
  ' "$file"
}

zig_version="$(zig version)"
if [[ "$zig_version" != "0.15.2" ]]; then
  printf 'Expected zig version 0.15.2, found %s\n' "$zig_version" >&2
  exit 1
fi

ghostty_repo="$(toml_value ghostty repo "$TOOLCHAIN_FILE")"
ghostty_ref="$(toml_value ghostty ref "$TOOLCHAIN_FILE")"
build_step="$(toml_value ghostty build_step "$TOOLCHAIN_FILE")"

if [[ "$build_step" != "lib-vt" ]]; then
  printf 'Expected ghostty.build_step to be lib-vt, found %s\n' "$build_step" >&2
  exit 1
fi

mkdir -p "$REPO_ROOT/.tools"

if [[ -d "$SOURCE_DIR/.git" ]]; then
  git -C "$SOURCE_DIR" remote set-url origin "$ghostty_repo"
  git -C "$SOURCE_DIR" fetch origin --prune --tags
else
  rm -rf "$SOURCE_DIR"
  git clone "$ghostty_repo" "$SOURCE_DIR"
fi

git -C "$SOURCE_DIR" checkout --force "$ghostty_ref"
git -C "$SOURCE_DIR" reset --hard "$ghostty_ref"

rm -rf "$INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

(cd "$SOURCE_DIR" && zig build "$build_step" --prefix "$INSTALL_DIR")

mapfile -t object_files < <(find "$SOURCE_DIR/.zig-cache/o" -type f -name '*.o' | sort)
if (( ${#object_files[@]} == 0 )); then
  printf 'No Zig object files were produced under %s\n' "$SOURCE_DIR/.zig-cache/o" >&2
  exit 1
fi

ar crs "$INSTALL_DIR/lib/libghostty-vt.a" "${object_files[@]}"

test -f "$INSTALL_DIR/include/ghostty/vt.h"
test -f "$INSTALL_DIR/lib/libghostty-vt.a"
test -f "$INSTALL_DIR/lib/libghostty-vt.so"
