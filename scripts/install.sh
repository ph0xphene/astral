#!/usr/bin/env bash
set -euo pipefail

DEFAULT_REPO_URL="https://github.com/merelinmrelin-web/astral.git"
SCRIPT_PATH="${BASH_SOURCE[0]:-$0}"
if [[ -n "$SCRIPT_PATH" && -f "$SCRIPT_PATH" ]]; then
  ROOT_DIR="$(cd "$(dirname "$SCRIPT_PATH")/.." && pwd)"
else
  ROOT_DIR="$(pwd)"
fi
PREFIX="${ASTRAL_PREFIX:-$HOME/.local}"
DRY_RUN=0
SKIP_PROTOBUF_INSTALL=0
REPO_URL="${ASTRAL_REPO_URL:-$DEFAULT_REPO_URL}"
WORK_DIR=""

usage() {
  cat <<'USAGE'
Usage: scripts/install.sh [options]

Build and install the astral CLI.

Options:
  --prefix <dir>              Install prefix (default: ~/.local)
  --dry-run                   Print actions without changing anything
  --skip-protobuf-install     Do not install protobuf automatically on macOS
  -h, --help                  Show this help

Examples:
  curl -fsSL https://raw.githubusercontent.com/merelinmrelin-web/astral/main/scripts/install.sh | bash
  scripts/install.sh
  scripts/install.sh --prefix /usr/local
  ASTRAL_PREFIX="$HOME/.local" scripts/install.sh
USAGE
}

log() {
  printf '==> %s\n' "$*"
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

run() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    printf 'DRY RUN: %s\n' "$*"
  else
    "$@"
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix)
      [[ $# -ge 2 ]] || die "--prefix requires a directory"
      PREFIX="$2"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --skip-protobuf-install)
      SKIP_PROTOBUF_INSTALL=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

BIN_DIR="$PREFIX/bin"
TARGET_BIN="$BIN_DIR/astral"

cleanup() {
  if [[ -n "$WORK_DIR" && -d "$WORK_DIR" ]]; then
    rm -rf "$WORK_DIR"
  fi
}
trap cleanup EXIT

if [[ ! -f "$ROOT_DIR/Cargo.toml" ]]; then
  if ! command -v git >/dev/null 2>&1; then
    die "git is required when installing from the one-line installer"
  fi
  WORK_DIR="$(mktemp -d)"
  log "cloning $REPO_URL"
  run git clone --depth 1 "$REPO_URL" "$WORK_DIR/astral"
  ROOT_DIR="$WORK_DIR/astral"
fi

log "install prefix: $PREFIX"
log "target binary: $TARGET_BIN"

if ! command -v cargo >/dev/null 2>&1; then
  die "Rust/Cargo is required. Install it from https://rustup.rs, then rerun this script."
fi

if ! command -v protoc >/dev/null 2>&1; then
  if [[ "$(uname -s)" == "Darwin" && "$SKIP_PROTOBUF_INSTALL" -eq 0 && -x /opt/homebrew/bin/brew ]]; then
    log "protobuf compiler not found; installing protobuf with Homebrew"
    run /opt/homebrew/bin/brew install protobuf
  elif [[ "$(uname -s)" == "Darwin" && "$SKIP_PROTOBUF_INSTALL" -eq 0 && -x /usr/local/bin/brew ]]; then
    log "protobuf compiler not found; installing protobuf with Homebrew"
    run /usr/local/bin/brew install protobuf
  else
    die "protoc is required. Install protobuf with your package manager, then rerun this script."
  fi
fi

log "building release binary"
run cargo build --release --bin astral

log "installing astral"
run mkdir -p "$BIN_DIR"
run cp "$ROOT_DIR/target/release/astral" "$TARGET_BIN"
run chmod +x "$TARGET_BIN"

if [[ "$DRY_RUN" -eq 1 ]]; then
  log "dry run complete"
else
  log "installed $("$TARGET_BIN" --version 2>/dev/null || echo astral) at $TARGET_BIN"
  if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
    printf '\nAdd this to your shell profile if needed:\n  export PATH="%s:$PATH"\n' "$BIN_DIR"
  fi
fi
