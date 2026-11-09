#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
omarchy_path="${OMARCHY_PATH:-/usr/share/omarchy}"

if [[ ! -f "$omarchy_path/shell/shell.qml" ]]; then
  printf 'Omarchy shell not found at %s (set OMARCHY_PATH)\n' "$omarchy_path" >&2
  exit 1
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/telecast-smoke.XXXXXX")
trap 'rm -rf -- "$tmp"' EXIT
ln -s "$omarchy_path/shell/Ui" "$tmp/Ui"
ln -s "$omarchy_path/shell/Commons" "$tmp/Commons"
cp "$root/tests/Smoke.qml" "$tmp/Smoke.qml"

CAST_SOURCE="$root" quickshell -p "$tmp/Smoke.qml"
