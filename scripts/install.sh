#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${ASTRAL_PREFIX:-$HOME/.local}"
DRY_RUN=0
SKIP_PROTOBUF_INSTALL=0

usage() {
  cat <<'USAGE'
Usage: scripts/install.sh [options]

Build and install the astral CLI from this checkout.

Options:
  --prefix <dir>              Install prefix (default: ~/.local)
  --dry-run                   Print actions without changing anything
  --skip-protobuf-install     Do not install protobuf automatically on macOS
  -h, --help                  Show this help

Examples:
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

[[ -f "$ROOT_DIR/Cargo.toml" ]] || die "run this script from an astral checkout"

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
