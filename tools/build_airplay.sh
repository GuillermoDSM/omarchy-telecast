#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
destination="${1:-$root/engine/target/release/doubletake}"
cache="${XDG_CACHE_HOME:-$HOME/.cache}/omarchy-miracast/doubletake"
commit=962b47f609f88098d809e0ef680280539bb685fd

if ! command -v go >/dev/null 2>&1; then
  printf 'Se necesita Go 1.25 o posterior para compilar DoubleTake.\n' >&2
  exit 1
fi
if ! command -v git >/dev/null 2>&1; then
  printf 'Se necesita git para obtener DoubleTake.\n' >&2
  exit 1
fi

mkdir -p -- "$(dirname -- "$destination")" "$(dirname -- "$cache")"
if [[ ! -d "$cache/.git" ]]; then
  git clone --filter=blob:none --no-checkout https://github.com/jmh-zero/doubletake.git "$cache"
fi
git -C "$cache" fetch --depth=1 origin "$commit"
git -C "$cache" checkout --detach "$commit"
git -C "$cache" reset --hard "$commit"
git -C "$cache" clean -fdx
git -C "$cache" apply --check "$root/third_party/doubletake/omarchy.patch"
git -C "$cache" apply "$root/third_party/doubletake/omarchy.patch"
(
  cd "$cache"
  go build -trimpath -ldflags='-s -w' -o "$destination" ./cmd/doubletake
)
chmod 755 -- "$destination"
printf 'DoubleTake compilado en %s\n' "$destination"
