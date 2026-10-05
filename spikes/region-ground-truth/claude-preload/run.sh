#!/bin/sh
# Run Claude Code with the region preload and an empty config dir, so it shows
# first-run onboarding and never touches the user's credentials or settings.
# Usage: run.sh CONFIG_DIR LOG_FILE
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$1"
export CLAUDE_CONFIG_DIR="$1" CC_REGIONS_LOG="$2"
export BUN_OPTIONS="--preload $here/cc-regions.cjs"
exec claude
