#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
destination="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/astro67.miracast"

if [[ -e "$destination" || -L "$destination" ]]; then
  printf 'El destino ya existe: %s\nNo se sobrescribirá automáticamente.\n' "$destination" >&2
  exit 1
fi

cargo build --manifest-path "$root/engine/Cargo.toml" --release --locked --jobs "${CARGO_BUILD_JOBS:-2}"
omarchy plugin validate "$root"

# Stage outside the watched plugins directory, then publish with one rename.
# No install hooks, sudo, services, firewall edits or changes under /usr/share.
parent=$(dirname -- "$destination")
mkdir -p -- "$parent"
stage=$(mktemp -d "${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/.miracast-install.XXXXXX")
trap 'if [[ -d "$stage" ]]; then rmdir -- "$stage/bin" 2>/dev/null || true; rmdir -- "$stage" 2>/dev/null || true; fi' EXIT
for file in manifest.json qmldir MiracastWidget.qml MiracastService.qml MiracastModel.js README.md LICENSE; do
  install -m 644 -- "$root/$file" "$stage/$file"
done
install -D -m 755 -- "$root/engine/target/release/omarchy-miracast" "$stage/bin/omarchy-miracast"
mv -T --no-clobber -- "$stage" "$destination"
if [[ -d "$stage" ]]; then
  printf 'El destino apareció durante la instalación; no se ha sobrescrito: %s\n' "$destination" >&2
  exit 1
fi

# Enabling uses Omarchy's own layout writer. Keep a snapshot of the existing
# user layout first; do not replace it with a hard-coded configuration.
shell_config="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/shell.json"
if [[ -f "$shell_config" ]]; then
  backup=$(mktemp "${shell_config}.before-miracast.XXXXXX")
  cp -p -- "$shell_config" "$backup"
  printf 'Copia de la distribución de la barra: %s\n' "$backup"
fi
omarchy-shell shell rescanPlugins
# Rescanning is asynchronous. Wait for discovery, as Omarchy's own plugin
# installer does, rather than racing enablePlugin immediately after rescan.
discovered=0
for (( attempt = 0; attempt < 40; attempt++ )); do
  if omarchy plugin list --json | jq -e 'any(.[]; .id == "astro67.miracast")' >/dev/null; then
    discovered=1
    break
  fi
  sleep 0.05
done
if (( ! discovered )); then
  printf 'Los archivos están instalados, pero el shell no terminó de descubrir el plugin.\n' >&2
  exit 1
fi
omarchy plugin enable astro67.miracast
printf 'Instalado: %s\n' "$destination"
