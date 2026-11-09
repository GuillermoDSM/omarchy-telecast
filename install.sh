#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
destination="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/astro67.telecast"
legacy="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/astro67.miracast"

if [[ -e "$legacy" || -L "$legacy" ]]; then
  printf 'Legacy plugin found: %s\nRemove it before installing Telecast: omarchy plugin remove astro67.miracast\n' "$legacy" >&2
  exit 1
fi

if [[ -e "$destination" || -L "$destination" ]]; then
  printf 'Destination already exists: %s\nIt will not be overwritten automatically.\n' "$destination" >&2
  exit 1
fi

cargo build --manifest-path "$root/engine/Cargo.toml" --release --locked --jobs "${CARGO_BUILD_JOBS:-2}"
omarchy plugin validate "$root"

# Stage outside the watched plugins directory, then publish with one rename.
# No install hooks, sudo, services, firewall edits or changes under /usr/share.
parent=$(dirname -- "$destination")
mkdir -p -- "$parent"
stage=$(mktemp -d "${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/.telecast-install.XXXXXX")
trap 'if [[ -d "$stage" ]]; then rm -rf -- "$stage"; fi' EXIT
for file in manifest.json qmldir TelecastWidget.qml TelecastService.qml TelecastModel.js README.md LICENSE; do
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
install -D -m 755 -- "$root/engine/target/release/omarchy-telecast" "$stage/bin/omarchy-telecast"
mv -T --no-clobber -- "$stage" "$destination"
if [[ -d "$stage" ]]; then
  printf 'Destination appeared during installation; nothing was overwritten: %s\n' "$destination" >&2
  exit 1
fi

# Enabling uses Omarchy's own layout writer. Keep a snapshot of the existing
# user layout first; do not replace it with a hard-coded configuration.
shell_config="${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/shell.json"
if [[ -f "$shell_config" ]]; then
  backup=$(mktemp "${shell_config}.before-telecast.XXXXXX")
  cp -p -- "$shell_config" "$backup"
  printf 'Saved the current bar layout to: %s\n' "$backup"
fi
omarchy-shell shell rescanPlugins
# Rescanning is asynchronous. Wait for discovery, as Omarchy's own plugin
# installer does, rather than racing enablePlugin immediately after rescan.
discovered=0
for (( attempt = 0; attempt < 40; attempt++ )); do
  if omarchy plugin list --json | jq -e 'any(.[]; .id == "astro67.telecast")' >/dev/null; then
    discovered=1
    break
  fi
  sleep 0.05
done
if (( ! discovered )); then
  printf 'Files are installed, but the shell did not finish discovering the plugin.\n' >&2
  exit 1
fi
omarchy plugin enable astro67.telecast
printf 'Installed: %s\n' "$destination"
