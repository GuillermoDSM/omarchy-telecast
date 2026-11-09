.pragma library

var labels = {
  es: {
    offline: "ENVIAR PANTALLA", idle: "LISTO", searching: "BUSCANDO",
    display: "MONITOR VIRTUAL", capture: "ELIGE PANTALLA",
    connecting: "CONECTANDO", pairing: "EMPAREJANDO",
    streaming: "ENVIANDO PANTALLA", stopping: "DESCONECTANDO", error: "ERROR"
  },
  en: {
    offline: "SEND SCREEN", idle: "READY", searching: "SEARCHING",
    display: "VIRTUAL MONITOR", capture: "PICK A SCREEN",
    connecting: "CONNECTING", pairing: "PAIRING",
    streaming: "SENDING SCREEN", stopping: "DISCONNECTING", error: "ERROR"
  }
}

var text = {
  es: {
    title: "Enviar pantalla",
    off: "Desactivado",
    disconnecting: "Desconectando",
    error: "Error",
    readyPick: "Listo · elige una TV",
    ready: "Listo",
    connected: "CONECTADO",
    tvPassword: "CONTRASEÑA DE LA TV",
    tvPin: "PIN DE LA TV",
    password: "Contraseña",
    pin: "PIN",
    sendCode: "Enviar código",
    searching: "Buscando...",
    noScreens: "No hay pantallas disponibles",
    hideDebug: "Ocultar depuración",
    debug: "Depuración",
    testPatternOn: "✓ Patrón de prueba (sin capturar pantalla)",
    testPattern: "Patrón de prueba (sin capturar pantalla)",
    log: "Registro: ",
    pairing: "Emparejando…",
    disconnectingStatus: "Desconectando…",
    sending: "Enviando pantalla",
    connecting: "Conectando…",
    disconnect: "Desconectar",
    connect: "Conectar",
    screenDisabled: "Envío de pantalla desactivado",
    waitingDisconnect: "Esperando a que termine la desconexión",
    engineStopped: "Motor detenido",
    engineExit: "El motor terminó con código ",
    engineExitHint: ". Abre Diagnóstico para ver detalles.",
    invalidResponse: "Respuesta inválida del motor; consulta el diagnóstico",
    phrases: [
      "Buscando señales",
      "Sincronizando pantallas",
      "Enviando píxeles",
      "Ajustando latencia",
      "Tejiendo paquetes",
      "Domando radios",
      "Puliendo códecs",
      "Conectando mundos"
    ]
  },
  en: {
    title: "Send screen",
    off: "Off",
    disconnecting: "Disconnecting",
    error: "Error",
    readyPick: "Ready · pick a TV",
    ready: "Ready",
    connected: "CONNECTED",
    tvPassword: "TV PASSWORD",
    tvPin: "TV PIN",
    password: "Password",
    pin: "PIN",
    sendCode: "Send code",
    searching: "Searching...",
    noScreens: "No screens available",
    hideDebug: "Hide debug",
    debug: "Debug",
    testPatternOn: "✓ Test pattern (no screen capture)",
    testPattern: "Test pattern (no screen capture)",
    log: "Log: ",
    pairing: "Pairing…",
    disconnectingStatus: "Disconnecting…",
    sending: "Sending screen",
    connecting: "Connecting…",
    disconnect: "Disconnect",
    connect: "Connect",
    screenDisabled: "Screen sending disabled",
    waitingDisconnect: "Waiting for disconnect to finish",
    engineStopped: "Engine stopped",
    engineExit: "The engine exited with code ",
    engineExitHint: ". Open Diagnostics for details.",
    invalidResponse: "Invalid response from the engine; check diagnostics",
    phrases: [
      "Scanning signals",
      "Syncing screens",
      "Sending pixels",
      "Tuning latency",
      "Weaving packets",
      "Taming radios",
      "Polishing codecs",
      "Connecting worlds"
    ]
  }
}

function language(localeName) {
  var name = String(localeName || "").toLowerCase()
  return name.indexOf("es") === 0 ? "es" : "en"
}

function dictionary(lang) {
  return text[lang] || text.en
}

function phaseLabel(phase, lang) {
  var table = labels[lang] || labels.en
  return table[phase] || table.offline
}

function plain(value, limit) {
  return String(value || "").replace(/[\x00-\x1f\x7f]/g, " ").slice(0, limit || 600)
}

function connectRequest(receiver, testPattern) {
  return {
    command: "connect", mac: receiver.mac,
    ip: receiver.ip, port: receiver.port, name: receiver.name, test: !!testPattern
  }
}

function peers(value) {
  if (!Array.isArray(value)) return []
  var result = []
  var seen = ({})
  for (var i = 0; i < Math.min(value.length, 64); i++) {
    var peer = value[i]
    if (!peer) continue
    var ip = plain(peer.ip, 64)
    var port = Math.max(0, Math.min(65535, Number(peer.port) || 0))
    if (!ip || port === 0) continue
    var mac = String(peer.mac || peer.device_id || ip).toUpperCase()
    var key = "airplay:" + mac
    if (seen[key]) continue
    seen[key] = true
    result.push({
      key: key,
      mac: mac,
      name: plain(peer.name, 120) || mac,
      model: plain(peer.model, 120),
      ip: ip,
      port: port
    })
  }
  result.sort(function (left, right) { return left.name.localeCompare(right.name) })
  return result
}
