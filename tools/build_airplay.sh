#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
destination="${1:-$root/engine/target/release/doubletake}"
commit=962b47f609f88098d809e0ef680280539bb685fd

if ! command -v go >/dev/null 2>&1; then
  printf 'Se necesita Go 1.25 o posterior para compilar DoubleTake.\n' >&2
  exit 1
fi
if ! command -v git >/dev/null 2>&1; then
  printf 'Se necesita git para obtener DoubleTake.\n' >&2
  exit 1
fi

mkdir -p -- "$(dirname -- "$destination")"
work=$(mktemp -d "${TMPDIR:-/tmp}/omarchy-doubletake-build.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
git init -q "$work/source"
git -C "$work/source" fetch --depth=1 https://github.com/jmh-zero/doubletake.git "$commit"
git -C "$work/source" checkout --detach "$commit"
git -C "$work/source" apply --check "$root/third_party/doubletake/omarchy.patch"
git -C "$work/source" apply "$root/third_party/doubletake/omarchy.patch"
(
  cd "$work/source"
  go build -trimpath -ldflags='-s -w' -o "$destination" ./cmd/doubletake
)
chmod 755 -- "$destination"
printf 'DoubleTake compilado en %s\n' "$destination"
