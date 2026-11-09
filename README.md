# Telecast

Experimental AirPlay screen sender for the [Omarchy](https://omarchy.org) bar,
written in Rust and integrated with Quickshell, Hyprland, PipeWire and the XDG
Desktop Portal.

Telecast casts the current desktop to an AirPlay receiver. The name is
protocol-neutral: only AirPlay is implemented today, but the project is not
tied to a single casting protocol.

> **Status: experimental.** The native AirPlay path is not a complete AirPlay
> implementation and has not been verified with every TV. See
> [Limitations](#limitations).

## Features

- Bar widget with incremental AirPlay receiver discovery.
- mDNS discovery, HAP PIN pairing and encrypted per-receiver credentials.
- Screen capture through the XDG Desktop Portal and PipeWire.
- Temporary headless output `OMARCHY-TELECAST-<pid>` to extend the desktop.
- The portal chooses the mode: select the virtual output to extend, or an
  existing monitor to mirror. The unused virtual output is removed.
- Later Hyprland layout changes (mirror/extended, display off) are detected and
  capture is restarted without dropping the AirPlay session.
- A dedicated `Omarchy-AirPlay-TV` audio sink. On connect it becomes the
  default output and existing app streams are moved there. On disconnect the
  previous output is restored unless the user chose another one.
- Audio is captured from that sink's monitor, never from the microphone.
- H.264 1080p30 encoding with x264.
- Initial bitrate of 8 Mb/s, automatic backoff to 3 Mb/s under real sender
  congestion, and gradual recovery when the link improves.
- Video sending runs separately from audio pacing, so a blocked TCP write does
  not cause audio dropouts.
- Ordered shutdown of the AirPlay session, portal, virtual output and audio.

## Requirements

- Omarchy with third-party plugin support.
- Hyprland with `hyprctl output create headless` and the Lua `hl.monitor` API.
- XDG Desktop Portal with a ScreenCast backend (`xdg-desktop-portal-hyprland`).
- PipeWire, `pipewire-pulse`, `pactl`, `pw-cli` and `pw-dump`.
- GStreamer with `pipewiresrc`, `pulsesrc`, `videoconvert`, `videoscale`,
  `videorate`, `h264parse`, `x264enc` and `appsink`.
- `jq` for `install.sh`.
- Rust 1.88 or newer to build the engine.
- Node.js and Python 3 only for the test suite.

On Arch/Omarchy, the usual runtime and build packages are:

```bash
sudo pacman -S --needed \
  rust cargo pkgconf \
  hyprland xdg-desktop-portal xdg-desktop-portal-hyprland \
  pipewire pipewire-pulse gst-plugin-pipewire \
  gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugins-ugly gst-libav \
  libpulse jq nodejs python
```

Quickshell ships with Omarchy. The engine uses x264; `gst-plugin-va` and VAAPI
are not required.

## Install

```bash
bash install.sh
```

The installer:

1. builds the release engine with `cargo build --locked`;
2. validates the plugin manifest;
3. stages the plugin outside the watched plugins directory;
4. installs the binary, QML files, license notices and corresponding engine
   source;
5. rescans the shell and enables `astro67.telecast`.

It refuses to overwrite an existing installation. To update, remove the
installed plugin first or replace its directory manually after taking a backup.
It also refuses to continue if the legacy `astro67.miracast` plugin is present.

### Migrating from `astro67.miracast`

```bash
omarchy plugin disable astro67.miracast
omarchy plugin remove astro67.miracast
bash install.sh
```

To keep the existing AirPlay pairing, copy the private key and credentials
before the first connect:

```bash
install -d -m 700 ~/.local/state/omarchy-telecast
cp -p ~/.local/state/omarchy-miracast/airplay-native-key ~/.local/state/omarchy-telecast/
cp -p ~/.local/state/omarchy-miracast/airplay-native-*.cred ~/.local/state/omarchy-telecast/
```

Otherwise telecast asks for the TV PIN again on the next connection.

## Usage

1. Enable the **Telecast** widget in the Omarchy bar.
2. Open the panel and pick an AirPlay receiver.
3. Enter the PIN shown by the TV the first time you connect.
4. In the portal, choose the virtual monitor to extend the desktop or an
   existing monitor to mirror it.
5. Audio moves automatically to `Omarchy-AirPlay-TV`.

The **Debug** section of the panel contains a synthetic test pattern (it does
not capture the screen), the availability checks, the current session log path
and recent history.

The same actions are available over IPC:

```bash
omarchy-shell astro67.telecast status
omarchy-shell astro67.telecast scan
omarchy-shell astro67.telecast stop
omarchy-shell astro67.telecast enable
omarchy-shell astro67.telecast disable
```

### Diagnostics

After a release build, the engine can be inspected directly:

```bash
engine/target/release/omarchy-telecast doctor
engine/target/release/omarchy-telecast scan
```

Session logs are written to
`~/.local/state/omarchy-telecast/sessions/`. They are private to the user and
contain state transitions and counters, not RTSP bodies, device names,
addresses, credentials or screen contents.

AirPlay credentials and the local encryption key are stored under
`~/.local/state/omarchy-telecast/` with `0600` permissions in a `0700`
directory.

## Build and test

```bash
cargo test --manifest-path engine/Cargo.toml --locked
cargo build --manifest-path engine/Cargo.toml --release --locked
node tests/model.test.cjs
python3 tests/audio_pipewire_test.py
qmllint TelecastWidget.qml TelecastService.qml
bash tests/smoke.sh
omarchy plugin validate .
```

- `tests/audio_pipewire_test.py` starts a private PipeWire server and never
  touches the desktop audio graph.
- `tests/smoke.sh` loads the QML in a temporary Quickshell configuration with
  Omarchy's `qs.Ui` and `qs.Commons` modules.
- The optional Hyprland display lifecycle test creates and removes one
  temporary output in the current session:

```bash
cargo test --manifest-path engine/Cargo.toml --locked -- --ignored local_display_lifecycle
```

## Project layout

```text
engine/                     Rust AirPlay engine and native protocol code
  src/airplay_native/       AirPlay protocol, HAP pairing, capture, timing
TelecastWidget.qml          Bar widget and panel
TelecastService.qml         Shared singleton: engine process and IPC
TelecastModel.js            UI model, localization and peer normalization
tests/                      Node, QML, PipeWire and model tests
install.sh                  Build, stage and enable the plugin
manifest.json               Omarchy plugin manifest
```

## How it works

Quickshell starts `omarchy-telecast serve` and exchanges one JSON object per
line over stdin/stdout. The engine owns discovery, pairing, capture and the
AirPlay session.

For screen capture, telecast creates a temporary headless Hyprland output and
opens an XDG Desktop Portal ScreenCast session. The portal selection decides
whether the desktop is extended to the virtual output or mirrored from an
existing monitor. Video is encoded as H.264 with x264 and sent over AirPlay;
audio is captured from the monitor of the dedicated TV sink and sent as ALAC.

## Limitations

- AirPlay support is experimental and not verified with all TVs.
- Receivers that require **FairPlay SAP/FPSAP** are rejected. The native engine
  does not implement that handshake. Receivers without FPSAP are the target of
  the current path, but physical TV behavior is not guaranteed.
- Only H.264 1080p30. No 4K, HEVC, HDCP or UIBC.
- The first time mirror mode is used in a session, the portal asks for
  permission to capture the laptop screen. Later switches reuse that
  permission for the rest of the session.
- Audio is exclusive: TV or laptop, not both at once.
- Automatic reconnection is not promised if the TV closes the session or
  changes networks.
- Do not expect perfect A/V sync on every receiver.
- Telecast does not modify UFW, the router, NetworkManager or the permanent
  Hyprland configuration.
- The Rust engine currently emits diagnostics in Spanish even when the QML
  labels are in English. UI labels follow the system locale (Spanish/English);
  set `TELECAST_LANG=es` or `TELECAST_LANG=en` to override it. The plugin name
  is a proper noun and does not change.
- Not affiliated with Apple, Google or the AirPlay, Google Cast or Miracast
  trademarks.

## Privacy

- No telemetry or network service beyond the local AirPlay connection.
- The AirPlay private key is kept in a separate `0600` file inside a `0700`
  directory. It protects against accidental disclosure, not against another
  process running as the same user.
- Credentials are encrypted at rest, but no system keyring is used.
- The capture portal only authorizes the selected PipeWire stream. The engine
  never enumerates cameras or unrelated PipeWire nodes.

## License

The plugin's own code is MIT-licensed. Portions of the AirPlay protocol code are
derived from DoubleTake and remain available under
LGPL-3.0-or-later. See
[`engine/src/airplay_native/NOTICE.md`](engine/src/airplay_native/NOTICE.md)
and
[`engine/src/airplay_native/LICENSE-LGPL-3.0`](engine/src/airplay_native/LICENSE-LGPL-3.0).

## Credits

- [DoubleTake](https://github.com/jmh-zero/doubletake) for the AirPlay packet
  formats and timing behavior that informed the native backend.
- The Omarchy and Quickshell projects for the shell and UI APIs.
