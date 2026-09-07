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

required_zig_version="$(toml_value zig version "$TOOLCHAIN_FILE")"
zig="$(command -v zig || true)"
if [[ -z "$zig" ]] || [[ "$("$zig" version)" != "$required_zig_version" ]]; then
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
  zig_dir="$REPO_ROOT/.tools/zig-$zig_arch-$zig_os-$required_zig_version"
  zig="$zig_dir/zig"
  if [[ ! -x "$zig" ]]; then
    archive="$zig_dir.tar.xz"
    checksum="$(toml_value zig_sha256 "$zig_arch-$zig_os" "$TOOLCHAIN_FILE")"
    [[ -n "$checksum" ]] || { echo "Missing Zig checksum" >&2; exit 1; }
    mkdir -p "$REPO_ROOT/.tools"
    curl --fail --location --retry 3 "https://ziglang.org/download/$required_zig_version/zig-$zig_arch-$zig_os-$required_zig_version.tar.xz" -o "$archive"
    if command -v sha256sum >/dev/null 2>&1; then
      actual_checksum="$(sha256sum "$archive" | awk '{print $1}')"
    else
      actual_checksum="$(shasum -a 256 "$archive" | awk '{print $1}')"
    fi
    [[ "$actual_checksum" == "$checksum" ]] || { echo "Zig checksum mismatch" >&2; exit 1; }
    tar -xJf "$archive" -C "$REPO_ROOT/.tools"
  fi
fi
zig_version="$("$zig" version)"
if [[ "$zig_version" != "$required_zig_version" ]]; then
  printf 'Expected Zig %s, found %s\n' "$required_zig_version" "$zig_version" >&2
  exit 1
fi

ghostty_repo="$(toml_value ghostty repo "$TOOLCHAIN_FILE")"
ghostty_ref="$(toml_value ghostty ref "$TOOLCHAIN_FILE")"
build_step="$(toml_value ghostty build_step "$TOOLCHAIN_FILE")"

mkdir -p "$REPO_ROOT/.tools"

if [[ ! -d "$SOURCE_DIR/.git" ]]; then
  git init "$SOURCE_DIR"
  git -C "$SOURCE_DIR" remote add origin "$ghostty_repo"
else
  git -C "$SOURCE_DIR" remote set-url origin "$ghostty_repo"
fi
git -C "$SOURCE_DIR" fetch --depth=1 origin "$ghostty_ref"
git -C "$SOURCE_DIR" checkout --detach --force "$ghostty_ref"
git -C "$SOURCE_DIR" reset --hard "$ghostty_ref"

rm -rf "$INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

# shellcheck disable=SC2086
(cd "$SOURCE_DIR" && "$zig" build $build_step --prefix "$INSTALL_DIR")

# Produce a co-located .dSYM for the dylib on macOS so the libghostty-vt frames
# symbolicate when profiling/debugging the embedding app. Zig leaves DWARF in the
# .zig-cache object files referenced by the dylib's debug map, so dsymutil must
# run from SOURCE_DIR (where .zig-cache lives) right after the build, before the
# cache is GC'd.
if [ "$(uname -s)" = "Darwin" ] && command -v dsymutil >/dev/null 2>&1; then
  for dylib in "$INSTALL_DIR"/lib/libghostty-vt*.dylib; do
    [ -f "$dylib" ] || continue
    (cd "$SOURCE_DIR" && dsymutil "$dylib") || echo "warning: dsymutil failed for $dylib" >&2
  done
fi

test -f "$INSTALL_DIR/include/ghostty/vt.h"

case "$(uname -s)" in
  Darwin) shared_lib="$INSTALL_DIR/lib/libghostty-vt.dylib" ;;
  *)      shared_lib="$INSTALL_DIR/lib/libghostty-vt.so" ;;
esac
static_lib="$INSTALL_DIR/lib/libghostty-vt.a"
if [ ! -f "$static_lib" ] && [ ! -f "$shared_lib" ]; then
  echo "missing Ghostty VT library: expected $static_lib or $shared_lib" >&2
  exit 1
fi
