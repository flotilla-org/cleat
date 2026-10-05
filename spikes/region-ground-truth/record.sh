#!/bin/sh
# Record a command under an isolated cleat daemon and copy its cast out.
# Usage: record.sh RUNTIME_DIR OUT.cast SECONDS CMD [ENV=VAL...]
# Starts a daemon rooted at RUNTIME_DIR (never the user's default daemon) and
# stops only daemons whose pid it learned from that runtime dir.
set -eu
rt=$1 out=$2 secs=$3 cmd=$4
shift 4
mkdir -p "$rt"
unset CLEAT_SESSION CLEAT_DAEMON
export CLEAT_RUNTIME_DIR="$rt"
envs=""
for kv in "$@"; do envs="$envs --env $kv"; done
id=gt$$
# shellcheck disable=SC2086
cleat launch "$id" --size 100x30 --record --cmd "$cmd" $envs >/dev/null
sleep "$secs"
cast=$(find "$rt" -path "*$id*" -name session.cast | head -n 1)
cp "$cast" "$out"
cleat kill "$id" >/dev/null 2>&1 || true
echo "recorded $out ($(wc -c < "$out") bytes)"
