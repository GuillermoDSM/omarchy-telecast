# Omarchy Enviar pantalla

Plugin `astro67.miracast`: emisor **AirPlay** experimental escrito en Rust para
la barra de Omarchy. El identificador del plugin se conserva por compatibilidad;
Miracast y DoubleTake ya no forman parte del proyecto.

## Implementado

- Widget de barra con búsqueda incremental de receptores AirPlay.
- Descubrimiento mDNS, emparejamiento por PIN y credenciales privadas por TV.
- Captura de pantalla mediante el portal XDG y PipeWire.
- Monitor virtual temporal `OMARCHY-WFD-<pid>` para extender el escritorio.
- El portal decide el modo: el monitor virtual extiende; una pantalla existente
  duplica. La pantalla virtual se retira si no fue la elegida.
- Cambios posteriores de Hyprland (mirror/extended o apagar la laptop) se
  detectan y reinician la captura sin cerrar la sesión AirPlay.
- Audio por una salida dedicada `Omarchy-AirPlay-TV`; al conectar se convierte
  en la salida predeterminada y las apps se mueven allí. Al desconectar se
  restaura la salida anterior, salvo que el usuario haya elegido otra.
- Captura de audio del monitor de esa salida, nunca del micrófono.
- Vídeo H.264 1080p30 con **VAAPI** (`vah264enc`) y respaldo a x264.
- Bitrate inicial de 8 Mb/s con descenso automático a 3 Mb/s si hay congestión
  real y recuperación gradual al mejorar la red.
- El envío de vídeo corre en una tarea separada del audio para que un bloqueo
  TCP no provoque microcortes de audio.
- Cierre ordenado de sesión, portal, salida virtual y audio.

## Límites

- AirPlay es experimental y no está verificado con todas las TVs.
- Los receptores que requieren **FairPlay SAP/FPSAP** se rechazan: el motor
  nativo todavía no implementa ese handshake. Samsung Tizen y otros receptores
  sin FPSAP sí están soportados por el camino actual.
- Solo H.264 1080p30. Sin 4K, HEVC, HDCP ni UIBC.
- La primera vez que uses mirror en una sesión, el portal pedirá autorizar la
  pantalla de la laptop. Después se alterna sin volver a preguntar durante esa
  sesión.
- El audio es exclusivo: TV o laptop. No se reproduce simultáneamente en ambos.
- No se promete reconexión automática si la TV cierra la sesión o cambia de red.
- No se modifican UFW, el router, NetworkManager ni la configuración permanente
  de Hyprland.

## Requisitos

Omarchy con plugins, Hyprland, portal ScreenCast de Hyprland, PipeWire,
GStreamer con `pipewiresrc`, `h264parse` y `pulsesrc`, y `pactl`.

Para VAAPI:

```text
gst-plugin-va
```

Si `vah264enc` no está disponible se usa `x264enc`.

## Compilar y comprobar

```bash
cargo test --manifest-path engine/Cargo.toml --locked
cargo build --manifest-path engine/Cargo.toml --release --locked
node tests/model.test.cjs
python3 tests/audio_pipewire_test.py
qmllint MiracastWidget.qml MiracastService.qml
omarchy plugin validate .
```

## Instalar

```bash
bash install.sh
```

El instalador se niega a sobrescribir una instalación existente. Para
actualizar, respalda primero el directorio del plugin y sustituye sus archivos.

## Uso

1. Activa el widget y elige la TV AirPlay.
2. Introduce el PIN que muestra la TV la primera vez.
3. Elige en el portal el monitor virtual para extender o una pantalla existente
   para duplicar.
4. El audio pasa automáticamente a `Omarchy-AirPlay-TV`.

`scan`, `stop`, `enable`, `disable` y `status` están disponibles por IPC:

```bash
omarchy-shell astro67.miracast status
omarchy-shell astro67.miracast scan
omarchy-shell astro67.miracast stop
```

## Diagnóstico

```bash
engine/target/release/omarchy-miracast doctor
engine/target/release/omarchy-miracast scan
```

Los registros privados por sesión se guardan en
`~/.local/state/omarchy-miracast/sessions/`. Las credenciales AirPlay se cifran
en `~/.local/state/omarchy-miracast/`.

## Licencia

El código propio del plugin es MIT. Las partes del protocolo AirPlay derivadas
de DoubleTake conservan LGPL-3.0-or-later; los avisos y el texto de licencia
están en `engine/src/airplay_native/NOTICE.md` y
`engine/src/airplay_native/LICENSE-LGPL-3.0`.
