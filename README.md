# Omarchy Enviar pantalla — experimental

Plugin `astro67.miracast`: emisores Wi-Fi Display y AirPlay experimental en Rust,
con DoubleTake parcheado como respaldo. El identificador
del plugin se conserva por compatibilidad.

**DoubleTake funcionó con la Samsung UTU7000 en pruebas anteriores; el backend Rust no se ha probado con esa TV. Miracast
falló en la negociación Wi-Fi Direct de esa misma TV.** Miracast se mantiene como
respaldo para receptores que no anuncien AirPlay. No es una implementación
certificada ni está verificada con todas las Samsung, LG y Sony.

El motor Miracast no usa FluxCast ni GNOME Network Displays. El backend AirPlay
Rust contiene código derivado de DoubleTake bajo LGPL-3.0-or-later; consulta
`engine/src/airplay_native/NOTICE.md`.

## Implementado

- Widget de barra con la misma estructura visual que el panel de Bluetooth de
  Omarchy: hero con icono, título, estado y switch; secciones de conectado y
  disponibles; filas, separadores, tooltips y navegación por teclado. No incluye
  la sección de dispositivos emparejados.
- Receptores AirPlay y Miracast, y diagnóstico por etapas.
- Búsqueda incremental: la lista se vacía al iniciar, los receptores aparecen
  en el widget mientras el motor sigue buscando, y se puede conectar en cuanto
  aparece uno. La búsqueda se cancela al iniciar la conexión y su ventana es de
  12 segundos.
- Backend AirPlay Rust por defecto: mDNS, PIN/HAP, vídeo H.264 y audio ALAC
  para receptores sin FPSAP. DoubleTake sigue disponible como respaldo explícito.
- El backend Rust crea `Omarchy-AirPlay-TV`; selecciona esa salida para enviar
  audio a la TV. No mueve flujos ni cambia explícitamente la salida predeterminada.
- Miracast captura la salida predeterminada y también suena en la laptop.
- Descubrimiento y conexión P2P por la API D-Bus de NetworkManager.
- Perfil temporal (`persist=volatile`), ligado a la vida del cliente D-Bus,
  sin ruta predeterminada ni DNS del receptor. No usa un cliente DHCP externo.
- Monitor extendido temporal `OMARCHY-WFD-<pid>` mediante Hyprland/Lua.
- Selector de pantalla XDG Desktop Portal; PipeWire remoto restringido.
- Negociación RTSP/WFD de vídeo sin protección de contenido y audio AAC
  estéreo a 48 kHz cuando la TV y el sistema lo permiten.
- La captura usa monitores de salida PulseAudio/PipeWire, no el micrófono.
  El patrón de vídeo AirPlay no captura pantalla; usa el monitor de TV para audio.
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

- Miracast usa audio AAC estéreo: si la TV solo anuncia LPCM o faltan componentes,
  se avisa y se intenta vídeo sin audio; algunas TV pueden rechazarlo.
- La búsqueda AirPlay se actualiza en cuanto mDNS ve la TV. Miracast depende del
  barrido P2P de NetworkManager, que puede tardar hasta 8 segundos en publicar
  el receptor; mientras tanto el widget sigue respondiendo y permite cancelar.
- AirPlay Rust captura únicamente su salida dedicada; Miracast y DoubleTake por
  defecto capturan la salida predeterminada. El audio Rust no se ha validado
  contra distorsión ni sincronización con una TV.
- La captura del audio de salida incluye todos los sonidos que se reproduzcan
  por ella, incluidas notificaciones. No usa el permiso de pantalla para audio.
- AirPlay Rust necesita `pw-cli`, `pw-dump`, `pipewire-pulse`, `pactl`,
  `x264enc`, `pipewiresrc` y `pulsesrc`. `doctor` consulta su disponibilidad.
- Por defecto AirPlay crea una pantalla virtual extendida temporal y abre el selector XDG
  Portal. Hay que elegir `OMARCHY-WFD-<pid>`; se rechaza cualquier otra pantalla.
  Para evitar restaurar una selección antigua, se solicita el permiso en cada
  conexión. Para el respaldo DoubleTake es necesario recompilar el parche incluido.
- La primera conexión AirPlay pide el PIN. Rust guarda credenciales cifradas y
  una clave aleatoria separada en el directorio de estado privado (0700, archivos
  0600). No usa un llavero del sistema: otro proceso del mismo usuario puede
  leer ambas; no compartas ni el directorio ni sus copias. DoubleTake usa su
  propio archivo `airplay-credentials.json` y no comparte estas credenciales.
- Si Tailscale desvía `192.168.1.0/24` por su interfaz, AirPlay no alcanza la
  TV aunque esté en la misma Wi-Fi. El plugin lo detecta y avisa; no desactiva
  Tailscale ni cambia rutas por su cuenta.
- El backend Rust rechaza receptores que anuncian FPSAP/FairPlay; prueba el
  respaldo DoubleTake. El PIN/HAP nativo no cuenta con auditoría de seguridad.
- El puerto del servicio AirPlay de la TV puede cambiar entre sesiones. El
  plugin usa el puerto anunciado por mDNS; si una búsqueda no la encuentra,
  conviene reintentar antes de concluir que la TV no está disponible.
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
- En mirror AirPlay se comparte la pantalla elegida en el portal, sin cambiar
  la disposición física de las pantallas; Miracast mantiene el modo extendido.

### Opciones experimentales AirPlay

- `OMARCHY_AIRPLAY_BACKEND=native` (o `rust`) selecciona Rust, el predeterminado.
  `OMARCHY_AIRPLAY_BACKEND=doubletake` selecciona el respaldo explícitamente;
  no hay fallback silencioso. Reinicia el motor tras cambiarlo.
- El selector **Pantalla AirPlay** del widget permite duplicar una pantalla
  elegida en el portal o extender mediante la pantalla temporal. La selección
  se conserva mientras funciona la shell y se envía con cada conexión; no
  cambia la configuración de Hyprland. Al cambiar de modo durante una sesión,
  el widget desconecta y reconecta; hay que elegir otra vez la pantalla en el
  portal. Miracast solo admite extensión. Para
  clientes del motor que omitan `display_mode`, `OMARCHY_AIRPLAY_DISPLAY`
  acepta `mirror` o `extended` (por defecto). Hay que recompilar el DoubleTake
  parcheado para evitar restaurar una selección antigua en mirror.
- AirPlay Rust crea la salida temporal `Omarchy-AirPlay-TV`; requiere `pw-cli` y
  `pw-dump`. Selecciónala en Sonido para enviar audio a la TV, o selecciona la
  laptop para devolverlo. El selector de Omarchy mueve los flujos de aplicación;
  el plugin no modifica la salida predeterminada por su cuenta. Es experimental
  y puede presentar distorsión.
- Para probar esa salida con DoubleTake, establece
  `OMARCHY_AIRPLAY_EXPERIMENTAL_TV_SINK=1`. Por defecto DoubleTake conserva la
  captura anterior de la salida predeterminada.

## Requisitos

Omarchy con plugins, Hyprland con `hyprctl output create headless` y `hl.monitor`,
NetworkManager con Wi-Fi P2P y Polkit, un portal ScreenCast de Hyprland y
PipeWire. AirPlay Rust necesita `pw-cli`, `pw-dump`, `pipewire-pulse`, `pactl`,
`x264enc` y `pulsesrc`. DoubleTake requiere además su binario parcheado y
`openh264enc`. Los componentes se consultan con `doctor`.

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
python3 tests/audio_pipewire_test.py
cargo build --manifest-path engine/Cargo.toml --locked
python3 tests/worker_test.py
cargo run --manifest-path engine/Cargo.toml -- self-test
cargo run --manifest-path engine/Cargo.toml -- doctor
omarchy plugin validate .
```

`tools/build_airplay.sh` descarga el commit fijado de DoubleTake, aplica
`third_party/doubletake/omarchy.patch` y deja el binario en
`engine/target/release/doubletake`. `install.sh` lo copia junto al motor. Si no
se compila, el backend Rust puede seguir funcionando, pero el respaldo
DoubleTake no estará disponible.

La prueba opcional del backend Rust usa únicamente un receptor de prueba local,
un bus D-Bus privado y servidores PipeWire/WirePlumber aislados, con monitores
ALSA/Bluetooth deshabilitados. Requiere el ejecutable `test-receiver` de
DoubleTake, `dbus-run-session`, `pipewire-pulse` y `wireplumber`:

```bash
AIRPLAY_TEST_RECEIVER=/ruta/al/test-receiver python3 tests/native_airplay_loopback_test.py
```

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
archivos de interfaz, los binarios y el código fuente del motor con su lockfile
en `source/engine`; no instala el árbol de compilación observado por el shell.
Las licencias y avisos se incluyen en `licenses/`. Desde ese directorio de
fuentes se puede recompilar el motor con `cargo build --release --locked`, usando
las dependencias y herramientas indicadas arriba. El motor se inicia al abrir
el widget y no configura autostart ni systemd.
Antes de habilitarlo, guarda una copia `shell.json.before-miracast.*` de la
distribución actual de la barra.

## Primera prueba con la TV

### AirPlay

El widget usa Rust por defecto. Para comprobar explícitamente el backend o
recuperar DoubleTake desde una terminal sin cambiar el shell en ejecución:

```bash
OMARCHY_AIRPLAY_BACKEND=native engine/target/debug/omarchy-miracast doctor
OMARCHY_AIRPLAY_BACKEND=doubletake engine/target/debug/omarchy-miracast doctor
OMARCHY_AIRPLAY_BACKEND=doubletake engine/target/debug/omarchy-miracast serve
```

El último comando acepta las solicitudes JSONL de la sección Control. Para
usar el respaldo desde el widget, la variable debe estar en el entorno del
proceso de Omarchy Shell que inicia el motor; cambiarla en una terminal no
modifica un shell ya iniciado. La selección Rust o DoubleTake no migra PIN ni
credenciales entre ambos.

Para volver al backend anterior, compila primero `tools/build_airplay.sh` y
ejecuta el motor con `OMARCHY_AIRPLAY_BACKEND=doubletake`. Para el widget esa
variable debe estar en el entorno de Omarchy Shell al iniciarse; cambiarla puede
requerir reiniciar el shell. No hay cambio automático de backend. Para volver a
una instalación anterior completa se necesita una copia previa del directorio
del plugin; el instalador no la crea ni sobrescribe una instalación existente.

1. Comprobar que la TV está en la misma Wi-Fi y que AirPlay está activado.
2. Abrir el widget y esperar a que aparezca la TV con la etiqueta **AIRPLAY**.
3. Seleccionarla. La primera vez, la TV muestra un PIN: introducirlo en el
   campo que aparece en el widget.
4. Elegir **Extender** o **Duplicar**, y seleccionar la pantalla indicada en el
   portal. En extensión, elegir `OMARCHY-WFD-<pid>`.
5. Seleccionar `Omarchy-AirPlay-TV` en las salidas de audio y reproducir sonido.
   Las aplicaciones con rutas propias pueden permanecer en la laptop. Cambiar
   a la salida de la laptop deja de enviar a la TV el audio que ya no va al sink.

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
`pin` lo envía. El estado `streaming` solo indica que el backend seleccionado
estableció la sesión; no confirma la reproducción en la TV.

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
