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

outside_checkout="$(mktemp -d)"
trap 'rm -rf "$outside_checkout"' EXIT

curl_style_output="$(cd "$outside_checkout" && bash -s -- --dry-run --prefix "$outside_checkout/prefix" < "$INSTALL_SH")"
case "$curl_style_output" in
  *"cloning https://github.com/ph0xphene/astral.git"*"/prefix/bin/astral"* ) ;;
  *)
    echo "expected curl-style dry-run output to clone astral and describe target binary path" >&2
    exit 1
    ;;
esac

echo "install script smoke tests passed"
