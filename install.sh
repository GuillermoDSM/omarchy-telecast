#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
destination="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/astro67.miracast"

if [[ -e "$destination" || -L "$destination" ]]; then
  printf 'El destino ya existe: %s\nNo se sobrescribirá automáticamente.\n' "$destination" >&2
  exit 1
fi

cargo build --manifest-path "$root/engine/Cargo.toml" --release --locked --jobs "${CARGO_BUILD_JOBS:-2}"
airplay_bin="$root/engine/target/release/doubletake"
if [[ ! -x "$airplay_bin" ]]; then
  if command -v go >/dev/null 2>&1; then
    bash "$root/tools/build_airplay.sh" "$airplay_bin"
  else
    printf 'Aviso: falta %s y Go no está instalado. El respaldo DoubleTake no estará disponible; AirPlay Rust sigue disponible.\n' "$airplay_bin" >&2
  fi
fi
omarchy plugin validate "$root"

# Stage outside the watched plugins directory, then publish with one rename.
# No install hooks, sudo, services, firewall edits or changes under /usr/share.
parent=$(dirname -- "$destination")
mkdir -p -- "$parent"
stage=$(mktemp -d "${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/.miracast-install.XXXXXX")
trap 'if [[ -d "$stage" ]]; then rm -rf -- "$stage"; fi' EXIT
for file in manifest.json qmldir MiracastWidget.qml MiracastService.qml MiracastModel.js README.md LICENSE; do
  install -m 644 -- "$root/$file" "$stage/$file"
done
install -d -m 755 -- "$stage/licenses"
install -m 644 -- "$root/engine/src/airplay_native/NOTICE.md" "$stage/licenses/AirPlay-Rust-NOTICE.md"
install -m 644 -- "$root/engine/src/airplay_native/LICENSE-LGPL-3.0" "$stage/licenses/AirPlay-Rust-LGPL-3.0"
# Ship the corresponding engine source and lockfile alongside the statically
# linked binary so recipients can rebuild it with changes to the LGPL parts.
install -d -m 755 -- "$stage/source/engine"
cp -R -- "$root/engine/src" "$stage/source/engine/src"
install -m 644 -- "$root/engine/Cargo.toml" "$root/engine/Cargo.lock" "$stage/source/engine/"
install -D -m 755 -- "$root/engine/target/release/omarchy-miracast" "$stage/bin/omarchy-miracast"
if [[ -x "$airplay_bin" ]]; then
  install -D -m 755 -- "$airplay_bin" "$stage/bin/doubletake"
fi
for file in LICENSE COPYING.GPL omarchy.patch README.md; do
  install -m 644 -- "$root/third_party/doubletake/$file" "$stage/licenses/DoubleTake-$file"
done
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
