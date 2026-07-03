#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SH="$ROOT_DIR/scripts/install.sh"

help_output="$("$INSTALL_SH" --help)"
case "$help_output" in
  *"Usage: scripts/install.sh"* ) ;;
  *)
    echo "expected help output to include usage" >&2
    exit 1
    ;;
esac

dry_run_output="$("$INSTALL_SH" --dry-run --prefix "$ROOT_DIR/.tmp/install-test")"
case "$dry_run_output" in
  *"DRY RUN"*"/.tmp/install-test/bin/astral"* ) ;;
  *)
    echo "expected dry-run output to describe target binary path" >&2
    exit 1
    ;;
esac

echo "install script smoke tests passed"
