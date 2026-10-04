#!/usr/bin/env bash
set -euo pipefail
# Run the real C caller against a fresh, private embedded runtime.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
clipboard_fixture_dir="$(mktemp -d)"
trap 'rm -rf "$clipboard_fixture_dir"' EXIT
cd "$repo_root"
cargo build -p cleat --locked --features ghostty-vt
clipboard_target_dir="${CARGO_TARGET_DIR:-$repo_root/target}/debug"
case "$(uname -s)" in
  Darwin) export DYLD_LIBRARY_PATH="$clipboard_target_dir:$repo_root/.tools/ghostty-install/lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" ;;
  *) export LD_LIBRARY_PATH="$clipboard_target_dir:$repo_root/.tools/ghostty-install/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" ;;
esac
"${CC:-cc}" -std=c11 -Wall -Wextra -Werror -I crates/cleat/include \
  crates/cleat/tests/fixtures/clipboard_abi.c -L "$clipboard_target_dir" \
  -lcleat -o "$clipboard_fixture_dir/clipboard-abi"
"$clipboard_fixture_dir/clipboard-abi" "$clipboard_fixture_dir/runtime"
