# Decisiones y hallazgos — Miracast experimental

## Objetivo y alcance

- Prioridad: estabilidad y baja latencia para clases o presentaciones de **cinco horas**; ajustar calidad después. La meta aún no está validada.
- Plugin propio para Omarchy, sin FluxCast ni servicios permanentes adicionales. AirPlay queda fuera; por ahora, vídeo sin audio ni HDCP.
- Conservar el Wi-Fi habitual; no cambiar kernel, firmware, router ni firewall sin evidencia y autorización. Regla temporal UFW TCP 7236 desde `192.168.49.1`: mantener durante las pruebas y revisar al final.

## Arquitectura

- Escritorio extendido mediante salida `headless` **nativa de Hyprland** y workspace normal; un workspace por sí solo no sustituye un monitor virtual. Captura autorizada por portal/PipeWire.
- Motor Rust con NetworkManager, GStreamer, RTSP/WFD y H.264 sobre RTP/UDP; máximo inicial 1080p30. Patrón sintético para aislar la captura del resto de la cadena.
- Preferir detener y volver a conectar a reiniciar el adaptador. Registro privado por sesión, sin imágenes ni credenciales.

## Evolución y evidencia

- La LG mostró el patrón y el escritorio extendido; el patrón duró unos 23 minutos, pero extender se congeló en varias pruebas. Samsung sigue pendiente: falló la asociación Wi-Fi Direct, sin causa raíz confirmada.
- UFW bloqueaba TCP 7236; la regla específica permitió conectar la LG. También se observó UDP bloqueado, posiblemente RTCP, pero no se ha identificado su contenido ni autorizado una regla adicional.
- Hubo `Out of buffers` del portal y, en otras sesiones, reinicios reales de `iwlwifi` con colas bloqueadas, también observados con FluxCast. No atribuir todos los fallos a Hyprland ni dar por resuelto el problema inalámbrico.
- El envío secuencial podía esperar hasta 32 × 80 ms y retrasar el control. La primera corrección descartaba inmediatamente ante `WouldBlock`: produjo miles de pérdidas y una caída de bitrate; además podía no recuperar la disponibilidad del socket. Se sustituyó por espera asíncrona y cola corta.
- Cola de usuario: máximo **32 datagramas o 50 ms**, descartando lo antiguo. No limita las colas del kernel, la red ni la latencia total; perder RTP puede corromper temporalmente la imagen, por lo que se solicita un fotograma clave.
- Adaptación: distinguir presión local de informes RTCP del receptor. Sin RTCP, la recuperación es lenta y limitada a 5 Mb/s; el éxito del envío local **no confirma recepción**. Las métricas registran captura, codificación, envío y descartes de cola.
- Prueba LG del 2 de noviembre, desde las 11:36: escritorio vacío; bajó de 5 a 2,109 Mb/s, se recuperó hasta 3,109 y volvió a bajar a 2,331. Seguía activa a las 11:52, recuperándose. Las bajadas registradas coinciden con descartes locales; sin RTCP no se puede atribuir la causa a pérdida remota ni evaluar calidad recibida automáticamente. A las 11:52 había 4.478 descartes acumulados y 25 `WouldBlock`. Resultado parcial, no prueba de cinco horas.
- Esa sesión terminó sobre las 12:23, tras unos 47 minutos, con `Connection reset by peer`: el mínimo registrado fue 1,5 Mb/s y los descartes locales llegaron a 18.845. Captura y codificación seguían avanzando; no se observaron reinicios de `iwlwifi` cerca del cierre. La causa del cierre remoto y de los descartes sigue sin confirmarse.
- Revisión de adaptación: el límite de cola cuenta RTP ya codificado, no fotogramas completos; el ciclo puede añadir 32 paquetes antes de intentar vaciarla, y un remanente puede provocar descartes incluso con el socket disponible. El objetivo del encoder no limita cada ráfaga. Sin RTCP, recuperación local no equivale a ancho de banda remoto recuperado. Priorizar medidas de cola por etapa, descarte previo a codificar, control de ráfagas y pruebas de congestión antes de cambiar umbrales. Referencias: [Miracast §4.10](https://www.wi-fi.org/system/files/Miracast_Specification_v2.3.pdf), [RFC 8888](https://www.rfc-editor.org/rfc/rfc8888), [RFC 8834](https://www.rfc-editor.org/rfc/rfc8834).

## Pendiente

Analizar cuándo ocurren los descartes y validar envío/cancelación bajo presión real; investigar los UDP bloqueados sin abrir reglas amplias. Continuar pruebas progresivas con LG, separar posibles reinicios de `iwlwifi` y validar después Samsung y una sesión real de cinco horas.
