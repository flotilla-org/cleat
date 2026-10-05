#!/bin/sh
# Vendor ratatui-core 0.1.0 + ratatui-widgets 0.3.0 outside the repo, apply the
# region-emitter patch, and symlink them into ratatui-demo/vendor (gitignored).
# Usage: prepare.sh [VENDOR_DIR]   (default: $TMPDIR/cleat-region-spike-vendor)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
vendor=${1:-${TMPDIR:-/tmp}/cleat-region-spike-vendor}
registry=$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/ | head -n 1)
if [ ! -d "$registry/ratatui-core-0.1.0" ] || [ ! -d "$registry/ratatui-widgets-0.3.0" ]; then
  (cd "$here/ratatui-demo" && cargo fetch)
  registry=$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/ | head -n 1)
fi
rm -rf "$vendor"
mkdir -p "$vendor"
cp -R "$registry/ratatui-core-0.1.0" "$vendor/ratatui-core"
cp -R "$registry/ratatui-widgets-0.3.0" "$vendor/ratatui-widgets"
(cd "$vendor" && patch -p1 < "$here/ratatui-regions.patch")
ln -sfn "$vendor" "$here/ratatui-demo/vendor"
echo "patched ratatui vendored in $vendor"
