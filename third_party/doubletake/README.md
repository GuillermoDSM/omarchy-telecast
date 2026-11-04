# DoubleTake para AirPlay

El backend AirPlay del plugin usa [DoubleTake](https://github.com/omarroth/doubletake),
un emisor AirPlay para Linux escrito en Go.

## Origen fijado

- Repositorio: `https://github.com/jmh-zero/doubletake`
- Commit base: `962b47f609f88098d809e0ef680280539bb685fd`
  (`Fall back to NTP when a PTP receiver names no timeline`)
- Parche del plugin: `omarchy.patch`

Ese commit contiene la corrección NTP necesaria para la Samsung UTU7000. El
parche añade:

- `-preserve-receiver-volume`, que evita enviar `SET_PARAMETER volume: 0 dB`
  y por tanto no cambia el volumen de la TV;
- tono de prueba a `volume=0.01`, para que el patrón de depuración no suene a
  máximo;
- las mismas opciones en el modo daemon y en sus pruebas.
- selección obligatoria de la salida virtual indicada por el plugin, sin
  restaurar capturas anteriores y rechazando otras salidas del portal.

Con `OMARCHY_AIRPLAY_EXPERIMENTAL_TV_SINK=1` el plugin pasa `-audio-source`
con el monitor de la salida temporal `Omarchy-AirPlay-TV`; por defecto captura
la salida predeterminada. El parche añade una rama de silencio para mantener activa
la captura cuando no hay aplicaciones en esa salida. El plugin no pausa el
transporte AirPlay ni cambia el volumen del receptor. Esta ruta no garantiza por
sí sola que todos los receptores reproduzcan audio sin distorsión.

El parche se aplica sobre el commit base y se compila con Go 1.25 o posterior.

## Compilar

```bash
tools/build_airplay.sh
```

El resultado queda en `engine/target/release/doubletake`. `install.sh` lo copia
junto al motor del plugin. También se puede indicar otro binario con
`OMARCHY_AIRPLAY_BIN`.

## Licencia

DoubleTake es software LGPL-3.0-or-later. Se distribuye con `LICENSE` y
`COPYING.GPL`. El parche `omarchy.patch` se publica bajo la misma licencia.
El código propio del plugin sigue bajo MIT.
