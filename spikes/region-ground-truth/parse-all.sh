#!/bin/sh
# Parse every spike cast into JSON lines of (frame cells, region rectangles).
# Usage: parse-all.sh OUT_DIR
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$1
mkdir -p "$out"
unset CLEAT_SESSION CLEAT_DAEMON CLEAT_RUNTIME_DIR
cd "$here/../.."
cargo build -q -p cleat --locked --example region_cast
for cast in "$here"/*.cast; do
  name=$(basename "$cast" .cast)
  target/debug/examples/region_cast "$cast" > "$out/$name.jsonl"
done
python3 - "$out" <<'EOF'
import json, pathlib, sys
for p in sorted(pathlib.Path(sys.argv[1]).glob("*.jsonl")):
    frames = [json.loads(line) for line in p.open()]
    sizes = sorted({len(f["regions"]) for f in frames})
    print(f"{p.stem}: {len(frames)} frames, regions per frame {sizes}")
EOF
