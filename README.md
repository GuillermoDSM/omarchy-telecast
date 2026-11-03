# Omarchy Enviar pantalla — experimental

Plugin `astro67.miracast`: emisor Wi-Fi Display propio en Rust para Miracast y
backend AirPlay basado en una versión parcheada de DoubleTake. El identificador
del plugin se conserva por compatibilidad.

**La Samsung UTU7000 funciona por AirPlay en las pruebas realizadas; Miracast
falló en la negociación Wi-Fi Direct de esa misma TV.** Miracast se mantiene como
respaldo para receptores que no anuncien AirPlay. No es una implementación
certificada ni está verificada con todas las Samsung, LG y Sony.

El motor Miracast no usa FluxCast ni GNOME Network Displays. AirPlay usa
DoubleTake, un emisor Go independiente con licencia LGPL-3.0-or-later.

## Implementado

- Widget de barra, receptores AirPlay y Miracast, y diagnóstico por etapas.
- Backend AirPlay mediante DoubleTake en modo daemon: descubrimiento mDNS,
  emparejamiento por PIN, credenciales privadas por TV y estado en vivo.
- El volumen de la TV no se modifica al iniciar AirPlay. La TV aparece como una
  salida PipeWire temporal y se selecciona sola; se puede volver al altavoz o a
  los auriculares desde el panel de audio normal de Omarchy. Al terminar se
  restaura la salida anterior si seguía seleccionada.
- Descubrimiento y conexión P2P por la API D-Bus de NetworkManager.
- Perfil temporal (`persist=volatile`), ligado a la vida del cliente D-Bus,
  sin ruta predeterminada ni DNS del receptor. No usa un cliente DHCP externo.
- Monitor extendido temporal `OMARCHY-WFD-<pid>` mediante Hyprland/Lua.
- Selector de pantalla XDG Desktop Portal; PipeWire remoto restringido.
- Negociación RTSP/WFD de vídeo sin protección de contenido y audio AAC
  estéreo a 48 kHz cuando la TV y el sistema lo permiten.
- Audio de la salida predeterminada mediante su monitor PulseAudio/PipeWire;
  no captura el micrófono. En AirPlay se crea una salida virtual que representa
  la TV, se selecciona como salida normal y DoubleTake captura su monitor; al
  elegir el altavoz en el panel de audio, la TV deja de recibir audio. En
  Miracast se sigue escuchando también en la laptop. El patrón de prueba genera
  un tono sintético en vez de capturar audio del escritorio.
- H.264/x264 → MPEG-TS → RTP/UDP, con un máximo de 1080p30.
  Si el receptor no permite 1080p30 se intenta 720p30 y después VGA60.
- Bitrate inicial 4 Mb/s, mínimo 1,5 y máximo 8. El descenso por presión
  local exige tres intervalos consecutivos y aplica una pausa de diez segundos.
  Sin RTCP, tras 30 segundos con hasta un 1 % de descartes locales por segundo
  se recuperan 250 kb/s,
  hasta un máximo de 4 Mb/s: eso **no verifica la recepción en la TV**.
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

- Solo audio AAC estéreo: si la TV solo anuncia LPCM o faltan componentes,
  se avisa y se intenta vídeo sin audio; algunas TV pueden rechazarlo.
- En AirPlay la TV es una salida temporal y se puede cambiar a cualquier otra
  salida desde el panel de audio de Omarchy, que mueve los flujos activos. Al
  terminar se retira la salida virtual. En Miracast se sigue escuchando también
  en la laptop y la captura queda ligada a la salida elegida al comenzar.
- La captura del audio de salida incluye todos los sonidos que se reproduzcan
  por ella, incluidas notificaciones. No usa el permiso de pantalla para audio.
- AirPlay necesita el binario `doubletake` parcheado, `openh264enc` y `pactl`.
  Si falta alguno, `doctor` lo indica y Miracast puede seguir funcionando.
- AirPlay abre su propio selector de pantalla (XDG Portal) además del widget.
  El monitor virtual de Miracast se puede elegir en ese selector, pero unificar
  ambos selectores requiere una integración más profunda.
- La primera conexión AirPlay pide el PIN que muestra la TV. Las credenciales
  se guardan en `~/.local/state/omarchy-miracast/airplay-credentials.json`
  con permisos privados; no las compartas.
- Si Tailscale desvía `192.168.1.0/24` por su interfaz, AirPlay no alcanza la
  TV aunque esté en la misma Wi-Fi. El plugin lo detecta y avisa; no desactiva
  Tailscale ni cambia rutas por su cuenta.
- AirPlay depende de DoubleTake (LGPL-3.0-or-later), no del motor Rust. Sus
  límites de códecs y latencia son los de ese emisor, no los del motor Miracast.
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
PipeWire. AirPlay necesita además el binario parcheado `doubletake`,
`openh264enc` y `pactl`. Los componentes se consultan con `doctor`.

Paquetes de ejecución habituales en Arch:

```text
networkmanager wpa_supplicant hyprland quickshell
xdg-desktop-portal xdg-desktop-portal-hyprland pipewire gst-plugin-pipewire
 gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugins-ugly
 gst-libav libpulse
```

Para compilar: Rust/Cargo, pkgconf y las bibliotecas de desarrollo que Arch
incluye en los paquetes de GStreamer/GLib. Para compilar DoubleTake desde el
parche: Go 1.25 o posterior y git. No se instalan servicios adicionales.
Dependencias Rust fijadas en `engine/Cargo.lock`.

## Compilar y comprobar

```bash
tools/build_airplay.sh
cargo test --manifest-path engine/Cargo.toml --locked
node tests/model.test.cjs
cargo build --manifest-path engine/Cargo.toml --locked
python3 tests/worker_test.py
cargo run --manifest-path engine/Cargo.toml -- self-test
cargo run --manifest-path engine/Cargo.toml -- doctor
omarchy plugin validate .
```

`tools/build_airplay.sh` descarga el commit fijado de DoubleTake, aplica
`third_party/doubletake/omarchy.patch` y deja el binario en
`engine/target/release/doubletake`. `install.sh` lo copia junto al motor. Si no
se compila, el plugin sigue mostrando Miracast pero AirPlay aparecerá como no
disponible en el diagnóstico.

La captura de audio requiere `pactl` (paquete `libpulse` en Arch), un servidor
PulseAudio o `pipewire-pulse` y un codificador AAC (`fdkaacenc`, `avenc_aac`
o `faac`). Si se negocia AAC pero no se puede abrir el monitor de salida, la
sesión termina con un error en lugar de capturar otra fuente.

`self-test` solo genera vídeo sintético local: comprueba H.264, MPEG-TS, RTP y
el cambio de bitrate, pero no una TV ni la captura real de Wayland. No abre
puertos de red ni solicita compartir la pantalla.

La prueba de integración incluida en `cargo test` añade un receptor simulado
en localhost: recorre OPTIONS/GET_PARAMETER/SET_PARAMETER/SETUP/PLAY/TEARDOWN,
se incorpora después de los primeros 600 ms, decodifica H.264 y AAC con
`avdec_h264` y `avdec_aac` (`gst-libav`), con tono sintético, y envía informes RTCP de pérdida
para comprobar que el bitrate del codificador baja realmente en ejecución.
No verifica interoperabilidad con una TV real. Se observó que durante el
arranque el demultiplexor simulado puede cambiar de programa antes de
estabilizarse ambos flujos; la incorporación tardía no valida ese caso inicial.

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

### AirPlay

1. Comprobar que la TV está en la misma Wi-Fi y que AirPlay está activado.
2. Abrir el widget y esperar a que aparezca la TV con la etiqueta **AIRPLAY**.
3. Seleccionarla. La primera vez, la TV muestra un PIN: introducirlo en el
   campo que aparece en el widget.
4. Elegir la pantalla en el selector XDG Portal que abre DoubleTake. Para
   escritorio extendido, elegir el monitor virtual si se ha creado.
5. Reproducir audio. La TV aparece en el panel de audio de Omarchy como una
   salida más y se selecciona automáticamente; el volumen y el mute de esa
   salida controlan el audio enviado. El volumen propio de la TV no se modifica.
   Puedes volver al altavoz o a los auriculares desde ese mismo panel; la TV
   dejará de recibir audio. Al detener se restaura la salida anterior si seguía
   seleccionada.

La primera conexión guarda credenciales privadas para no repetir el PIN.

### Miracast (respaldo)

1. Poner la TV en Compartir pantalla/Miracast, no AirPlay.
2. Abrir el widget y seleccionar la TV con la etiqueta **MIRACAST**.
3. Elegir `OMARCHY-WFD-<pid>` en el portal y confirmar la imagen.
4. Seleccionar la TV conectada para desconectar. Clic central en el icono
   también detiene la sesión. La sección **Depuración** permite activar el
   patrón de prueba, que no captura el escritorio ni crea un monitor. Si se
   negocia AAC, envía un tono audible: baja el volumen de la TV antes de probar.

El estado «Enviando pantalla» significa que la sesión de transporte se
estableció y salieron datos; **no afirma por sí solo que el televisor los haya
decodificado, mostrado o reproducido**. La comprobación visual y auditiva en la
TV sigue siendo necesaria.

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
{"command":"connect","protocol":"airplay","mac":"68:72:C3:A8:40:DF","ip":"192.168.1.139","port":59757,"name":"Samsung 7 Series (50)","test":false}
{"command":"pin","pin":"1234"}
{"command":"connect","protocol":"miracast","mac":"BE:7E:8B:BE:29:2E","test":true}
{"command":"stop"}
{"command":"quit"}
```

`scan` devuelve receptores de ambos protocolos con el campo `protocol`.
Durante AirPlay, el estado `pairing` indica que la TV espera el PIN; el comando
`pin` lo envía. El estado `streaming` solo confirma que DoubleTake aceptó la
sesión y comenzó a enviar.

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

AirPlay escribe su propio registro en
`~/.local/state/omarchy-miracast/airplay.log` (0600) y las credenciales de
emparejamiento en `~/.local/state/omarchy-miracast/airplay-credentials.json`
(0600). El registro puede contener el nombre, modelo e IP de la TV, pero no las
credenciales ni el PIN. El archivo de credenciales sí es secreto: no lo
compartas ni lo incluyas en informes.

El diagnóstico de cola distingue descartes por antigüedad y desbordamiento,
incluye su ocupación máxima y antigüedad actual, tamaño máximo de datagrama,
solicitudes locales de imagen clave y tiempo desde `WouldBlock` hasta el
siguiente envío exitoso. Son contadores agregados, no contenido de paquetes;
la antigüedad solo mide la cola del motor. Las solicitudes de imagen clave no
demuestran cuándo se codificó ni el tamaño de la imagen resultante.

## Retirar

Detener primero cualquier transmisión y después:

```bash
omarchy plugin remove astro67.miracast
```

No quedan servicios ni reglas del firewall creados por el plugin.

## Fuentes técnicas

El motor Miracast es código propio. AirPlay usa DoubleTake, fijado en el commit
`962b47f609f88098d809e0ef680280539bb685fd` del repositorio
[jmh-zero/doubletake](https://github.com/jmh-zero/doubletake), con el parche
`third_party/doubletake/omarchy.patch`. Ese commit incluye el respaldo NTP que
permite la sesión con la Samsung UTU7000. El parche evita cambiar el volumen de
la TV y reduce el tono de prueba.

Las estructuras de interoperabilidad se basan en documentación de Wi-Fi
Display/RTSP/RTP y las API públicas de los siguientes componentes:

- [DoubleTake](https://github.com/omarroth/doubletake) (LGPL-3.0-or-later)
- [NetworkManager D-Bus](https://networkmanager.dev/docs/api/latest/)
- [Hyprland: salidas virtuales](https://wiki.hypr.land/Configuring/Using-hyprctl/)
- [XDG ScreenCast](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
- [GStreamer](https://gstreamer.freedesktop.org/documentation/)
- RFC 2326 (RTSP), RFC 3550 (RTP/RTCP), RFC 2250 (MPEG-TS sobre RTP).

Licencia del código propio: MIT. DoubleTake y su parche se distribuyen bajo
LGPL-3.0-or-later; las licencias están en `third_party/doubletake/`. Los
componentes del sistema y dependencias conservan sus respectivas licencias; en
particular x264 no es código MIT.
