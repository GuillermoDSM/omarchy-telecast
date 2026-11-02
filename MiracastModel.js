.pragma library

function phaseLabel(phase) {
  var labels = {
    offline: "MIRACAST", idle: "LISTO", searching: "BUSCANDO",
    display: "MONITOR VIRTUAL", capture: "ELIGE PANTALLA",
    p2p: "WI-FI DIRECT", rtsp: "ESPERANDO TV", negotiating: "NEGOCIANDO",
    starting: "INICIANDO VÍDEO", streaming: "ENVIANDO VÍDEO",
    stopping: "DESCONECTANDO", error: "ERROR"
  }
  return labels[phase] || "MIRACAST"
}

function plain(value, limit) {
  return String(value || "").replace(/[\x00-\x1f\x7f]/g, " ").slice(0, limit || 600)
}

function peers(value) {
  if (!Array.isArray(value)) return []
  var result = []
  var seen = ({})
  for (var i = 0; i < Math.min(value.length, 64); i++) {
    var peer = value[i]
    if (!peer || !peer.wfd) continue
    var mac = String(peer.mac || "").toUpperCase()
    if (!/^([0-9A-F]{2}:){5}[0-9A-F]{2}$/.test(mac) || seen[mac]) continue
    seen[mac] = true
    result.push({
      mac: mac,
      name: plain(peer.name, 120) || mac,
      model: plain(peer.model, 120),
      strength: Math.max(0, Math.min(100, Number(peer.strength) || 0))
    })
  }
  return result
}

function bitrate(kbps) {
  var value = Number(kbps)
  return isFinite(value) && value > 0 ? (value / 1000).toFixed(2) + " Mb/s" : "—"
}
