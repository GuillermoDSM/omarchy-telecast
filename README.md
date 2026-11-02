# Omarchy Miracast — experimental

Plugin `astro67.miracast` y emisor Wi-Fi Display propio en Rust. No ejecuta
FluxCast, GNOME Network Displays ni DoubleTake. AirPlay no está implementado.

**No es una implementación Miracast certificada ni una solución de proyección
ya verificada con la Samsung UTU7000.** El grupo Wi-Fi Direct falló con esa TV
en pruebas previas con otros emisores; un motor nuevo no corrige necesariamente
el controlador, el firmware ni las limitaciones de NetworkManager.

## Implementado

- Widget de barra, receptores y diagnóstico por etapas.
- Descubrimiento y conexión P2P por la API D-Bus de NetworkManager.
- Perfil temporal (`persist=volatile`), ligado a la vida del cliente D-Bus,
  sin ruta predeterminada ni DNS del receptor. No usa un cliente DHCP externo.
- Monitor extendido temporal `OMARCHY-WFD-<pid>` mediante Hyprland/Lua.
- Selector de pantalla XDG Desktop Portal; PipeWire remoto restringido.
- Negociación RTSP/WFD de vídeo sin protección de contenido ni audio.
- H.264/x264 → MPEG-TS → RTP/UDP, con un máximo de 1080p30.
  Si el receptor no permite 1080p30 se intenta 720p30 y después VGA60.
- Bitrate inicial 5 Mb/s, mínimo 1,5 y máximo 8. El descenso por presión
  local exige tres intervalos consecutivos y aplica una pausa de diez segundos.
  Sin RTCP, tras 60 segundos de envíos locales sin errores se recuperan 250 kb/s,
  hasta un máximo de 5 Mb/s: eso **no verifica la recepción en la TV**.
  Con RTCP reciente se usan los informes del receptor para subir. Se cambia
  `x264enc.bitrate` en PLAYING.
- La cola de envío en espacio de usuario admite hasta 32 datagramas RTP y
  descarta los que alcanzan 50 ms de antigüedad, priorizando vídeo reciente.
  Estos límites no incluyen otras colas del sistema, la red o la TV y no
  garantizan la latencia total. Perder paquetes RTP puede corromper la imagen
  temporalmente; se solicita una imagen clave para facilitar la recuperación.
- Patrón sintético para separar errores del protocolo y de la captura.
- Cancelación, tiempos límite y cierre del perfil, encoder, portal y monitor.
- Un solo controlador QML compartido por las barras de todos los monitores.

## Límites importantes

- **Vídeo sin audio.** Algunos televisores pueden rechazar sesiones sin audio.
- Sin HDCP, UIBC, PIN WPS, transporte TCP de medios ni extensiones de Microsoft.
- Solo se negocian formatos H.264 CEA compatibles; no 4K/HEVC.
- Codificación inicial por software. No requiere instalar `gst-plugin-va`.
- Sin RTCP no se puede conocer la pérdida remota usando solo envíos UDP
  correctos. En ese caso no se promete adaptación completa; la recuperación
  local limitada no confirma recepción. La interfaz muestra informes recientes.
- El selector pide elegir el monitor virtual. No se saltan los permisos del
  portal ni se elige automáticamente un nodo PipeWire del sistema.
  Se identifica la salida por `mapping_id` cuando está disponible: el portal
  de Hyprland informa coordenadas `(0,0)` también para pantallas secundarias.
- Se cancela si cambia una conexión Wi-Fi que estaba activa al comenzar.
  **Esto no garantiza que el hardware no produzca una interrupción previa.**
- No se cambia UFW, el router, NetworkManager, `wpa_supplicant` ni archivos
  permanentes de Hyprland. Un bloqueo de TCP 7236 o RTCP requiere diagnóstico
  y autorización independiente para cualquier regla de firewall.
- Al cerrar se migran los espacios normales del monitor virtual a una pantalla
  existente. Nunca se elimina una salida que no pertenezca a esta sesión.
  Un SIGKILL no permite ejecutar esa limpieza: el perfil P2P cae por D-Bus,
  pero puede ser necesario retirar manualmente el monitor virtual.
- La duplicación se deja a las opciones nativas de pantallas de Omarchy;
  el plugin solo abre ese panel, no implementa un segundo modo mirror.

## Requisitos

Omarchy con plugins, Hyprland con `hyprctl output create headless` y `hl.monitor`,
NetworkManager con Wi-Fi P2P y Polkit, un portal ScreenCast de Hyprland y
PipeWire. Los componentes GStreamer requeridos se consultan con `doctor`.

Paquetes de ejecución habituales en Arch:

```text
networkmanager wpa_supplicant hyprland quickshell
xdg-desktop-portal xdg-desktop-portal-hyprland pipewire gst-plugin-pipewire
gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugins-ugly
```

Para compilar: Rust/Cargo, pkgconf y las bibliotecas de desarrollo que Arch
incluye en los paquetes de GStreamer/GLib. No instalar servicios adicionales.
Dependencias Rust fijadas en `engine/Cargo.lock`.

## Compilar y comprobar

```bash
cargo test --manifest-path engine/Cargo.toml --locked
node tests/model.test.cjs
cargo build --manifest-path engine/Cargo.toml --locked
python3 tests/worker_test.py
cargo run --manifest-path engine/Cargo.toml -- self-test
cargo run --manifest-path engine/Cargo.toml -- doctor
omarchy plugin validate .
```

`self-test` solo genera vídeo sintético local: comprueba H.264, MPEG-TS, RTP y
el cambio de bitrate, pero no una TV ni la captura real de Wayland. No abre
puertos de red ni solicita compartir la pantalla.

La prueba de integración incluida en `cargo test` añade un receptor simulado
en localhost: recorre OPTIONS/GET_PARAMETER/SET_PARAMETER/SETUP/PLAY/TEARDOWN,
decodifica H.264 con `avdec_h264` (`gst-libav`) y envía informes RTCP de pérdida
para comprobar que el bitrate del codificador baja realmente en ejecución.
No verifica interoperabilidad con una TV real.

La prueba opcional `display::tests::local_display_lifecycle` está ignorada por
defecto porque crea una salida temporal en la sesión Hyprland del usuario.
Verifica 1080p/30 Hz, que las pantallas existentes no cambien y que la salida
desaparezca al cerrar. Los tests de `worker_test.py` no buscan ni conectan TV.

Instalación local y activación en la barra, sin privilegios:

```bash
bash install.sh
```

El instalador se niega a sobrescribir un directorio existente. Instala solo los
archivos de interfaz y el binario, no el árbol de compilación observado por el
shell. El motor se inicia al abrir el widget y no configura autostart ni systemd.
Antes de habilitarlo, guarda una copia `shell.json.before-miracast.*` de la
distribución actual de la barra.

## Primera prueba con la TV

1. Poner la TV en Compartir pantalla/Miracast, no AirPlay.
2. Abrir el widget y pulsar **Buscar TV**.
3. Seleccionar el receptor. Conviene empezar con **Patrón de prueba**: no
   captura el escritorio ni crea un monitor.
4. Conectar y aceptar en la TV. Consultar el error por etapa si falla.
5. Solo después probar **Extender escritorio** y elegir `OMARCHY-WFD-<pid>`
   en el selector del portal. Confirmar visualmente la imagen en la TV.
6. **Detener** libera la sesión. Clic central en el icono también detiene.

El estado «Enviando vídeo» significa que PLAY fue aceptado y salieron paquetes;
**no afirma que el televisor los haya decodificado o mostrado**.

## Control y diagnóstico

```bash
omarchy-shell astro67.miracast status
omarchy-shell astro67.miracast scan
omarchy-shell astro67.miracast stop
```

El backend ofrece `serve` (JSONL por stdin/stdout), `doctor`, `scan` y
`self-test`. Ejemplos de mensajes de `serve`:

```json
{"command":"scan"}
{"command":"connect","mac":"BE:7E:8B:BE:29:2E","test":true}
{"command":"stop"}
{"command":"quit"}
```

No hay socket de escucha de control en la LAN ni contraseñas en argumentos.
El historial del widget es limitado y reside en memoria. Cada conexión crea
además un archivo JSONL privado (0600) en `~/.local/state/omarchy-miracast/sessions/`
(o `$XDG_STATE_HOME/omarchy-miracast/sessions/`). Guarda horas Unix en
milisegundos, etapas RTSP, código de error del sistema, resolución, bitrate,
contadores y estado RTCP; no guarda nombres, MAC, IP, credenciales, cuerpos
RTSP ni imágenes. El archivo se limita a 3600 registros y las métricas se
muestrean cada 10 segundos. No hay limpieza automática: revísalos y elimínalos
cuando ya no los necesites. El panel Diagnóstico indica el archivo de la sesión.
Si no se puede crear de forma privada, se muestra un aviso y la conexión sigue
sin registro. Los nuevos registros no contienen datos de sesiones anteriores.

## Retirar

Detener primero cualquier transmisión y después:

```bash
omarchy plugin remove astro67.miracast
```

No quedan servicios ni reglas del firewall creados por el plugin.

## Fuentes técnicas

Código propio. Las estructuras de interoperabilidad se basan en documentación
de Wi-Fi Display/RTSP/RTP y las API públicas de los siguientes componentes:

- [NetworkManager D-Bus](https://networkmanager.dev/docs/api/latest/)
- [Hyprland: salidas virtuales](https://wiki.hypr.land/Configuring/Using-hyprctl/)
- [XDG ScreenCast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
- [GStreamer](https://gstreamer.freedesktop.org/documentation/)
- RFC 2326 (RTSP), RFC 3550 (RTP/RTCP), RFC 2250 (MPEG-TS sobre RTP).

Licencia del código propio: MIT. Los componentes del sistema y dependencias
conservan sus respectivas licencias; en particular x264 no es código MIT.
