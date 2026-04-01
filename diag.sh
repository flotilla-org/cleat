#!/usr/bin/env bash
set -euo pipefail

export DYLD_LIBRARY_PATH="/Users/robert/dev/cleat.force-attach/.tools/ghostty-install/lib"
CLEAT="/Users/robert/dev/cleat.force-attach/target/debug/cleat"
SESSION_ID="diag-obo-$$"

cleanup() {
    echo "--- cleaning up ---"
    $CLEAT kill "$SESSION_ID" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Launching session $SESSION_ID ==="
$CLEAT launch --record --cmd "bash --norc --noprofile" "$SESSION_ID"
sleep 2

echo "=== Setting marker ==="
MARK_OUTPUT=$($CLEAT mark "$SESSION_ID" test-mark)
echo "Mark output: $MARK_OUTPUT"
OFFSET=$(echo "$MARK_OUTPUT" | grep -o '[0-9]\+')
echo "Marker offset: $OFFSET"

sleep 0.5

echo "=== Sending 'echo hello' ==="
$CLEAT send "$SESSION_ID" 'echo hello'
sleep 1

echo "=== Capture since marker (raw) ==="
RAW_OUTPUT=$($CLEAT capture "$SESSION_ID" --since-marker test-mark --raw)
echo "Raw output: >>>$RAW_OUTPUT<<<"

echo "=== Capture since numeric offset (raw) ==="
RAW_OUTPUT2=$($CLEAT capture "$SESSION_ID" --since "$OFFSET" --raw)
echo "Numeric offset output: >>>$RAW_OUTPUT2<<<"

# Examine the cast file
CAST_FILE="$TMPDIR/cleat-$(id -u)/$SESSION_ID/session.cast"
echo ""
echo "=== Cast file: $CAST_FILE ==="
echo "=== File size: $(wc -c < "$CAST_FILE") ==="
echo ""

echo "=== Lines around offset $OFFSET ==="
python3 -c "
import sys
offset = $OFFSET
with open('$CAST_FILE', 'rb') as f:
    content = f.read()
pos = 0
lines = content.split(b'\n')
for i, line in enumerate(lines):
    end = pos + len(line) + 1  # +1 for the newline
    marker = ''
    if pos == offset:
        marker = '  <-- OFFSET EXACT START'
    elif pos < offset < end:
        marker = f'  <-- OFFSET {offset} LANDS INSIDE (at char {offset - pos})'
    print(f'  byte {pos:6d}-{end-1:6d}: {line[:200]}{marker}')
    pos = end
"

echo ""
echo "=== Byte at offset $OFFSET ==="
python3 -c "
with open('$CAST_FILE', 'rb') as f:
    f.seek($OFFSET)
    byte = f.read(1)
    print(f'Byte at offset: {byte!r} (0x{byte[0]:02x})')
    f.seek($OFFSET)
    rest = f.readline()
    print(f'Line from offset: {rest[:200]!r}')
    # Also show the 5 bytes before the offset
    if $OFFSET > 0:
        f.seek($OFFSET - 5)
        before = f.read(5)
        print(f'5 bytes before offset: {before!r}')
"

echo ""
echo "=== Done ==="
