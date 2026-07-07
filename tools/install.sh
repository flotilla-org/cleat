#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${CLEAT_INSTALL_DIR:-$HOME/.local}"
BIN_DIR="$INSTALL_DIR/bin"
LIB_DIR="$INSTALL_DIR/lib"

# Build ghostty-vt if needed
"$REPO_ROOT/tools/prepare-ghostty-vt.sh"

GHOSTTY_PREFIX="$REPO_ROOT/.tools/ghostty-install"

# Build cleat with ghostty-vt
echo "Building cleat..."
(cd "$REPO_ROOT" && \
  CLEAT_GHOSTTY_PREFIX="$GHOSTTY_PREFIX" \
  cargo build -p cleat --locked --features ghostty-vt --release)

# Install binary and, if dynamic Ghostty linkage is used, the dylib
mkdir -p "$BIN_DIR" "$LIB_DIR"

cp "$REPO_ROOT/target/release/cleat" "$BIN_DIR/cleat"

if [[ ! -f "$GHOSTTY_PREFIX/lib/libghostty-vt.a" ]]; then
  case "$(uname -s)" in
    Darwin) DYLIB="libghostty-vt.dylib" ;;
    Linux)  DYLIB="libghostty-vt.so" ;;
    *)      echo "Unsupported OS" >&2; exit 1 ;;
  esac

  cp "$GHOSTTY_PREFIX/lib/$DYLIB" "$LIB_DIR/$DYLIB"

  # On macOS, add rpath so the binary finds the dylib in ../lib relative to itself
  if [[ "$(uname -s)" == "Darwin" ]]; then
    install_name_tool -add_rpath "@executable_path/../lib" "$BIN_DIR/cleat" 2>/dev/null || true
  fi

  echo "Installed $DYLIB to $LIB_DIR/$DYLIB"
fi

echo "Installed cleat to $BIN_DIR/cleat"

# Install the cleat-sessions agent-skill adapter to both skill locations:
# ~/.claude/skills (Claude Code) and ~/.agents/skills (the agentskills.io
# shared dir that pi and codex read). The repo copy is canonical; this
# owns sync from here (decision log 2026-07-06).
SKILL_SRC="$REPO_ROOT/docs/skills/cleat-sessions/SKILL.md"
if [[ -f "$SKILL_SRC" ]]; then
  for SKILLS_DIR in "$HOME/.claude/skills" "$HOME/.agents/skills"; do
    mkdir -p "$SKILLS_DIR/cleat-sessions"
    cp "$SKILL_SRC" "$SKILLS_DIR/cleat-sessions/SKILL.md"
    echo "Installed cleat-sessions skill to $SKILLS_DIR/cleat-sessions/"
  done
fi
