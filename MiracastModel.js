.pragma library

function phaseLabel(phase) {
  var labels = {
    offline: "ENVIAR PANTALLA", idle: "LISTO", searching: "BUSCANDO",
    display: "MONITOR VIRTUAL", capture: "ELIGE PANTALLA",
    p2p: "WI-FI DIRECT", rtsp: "ESPERANDO TV", negotiating: "NEGOCIANDO",
    connecting: "CONECTANDO", pairing: "EMPAREJANDO",
    starting: "INICIANDO VÍDEO", streaming: "ENVIANDO PANTALLA",
    stopping: "DESCONECTANDO", error: "ERROR"
  }
  return labels[phase] || "ENVIAR PANTALLA"
}

function plain(value, limit) {
  return String(value || "").replace(/[\x00-\x1f\x7f]/g, " ").slice(0, limit || 600)
}

function protocolLabel(protocol) {
  return protocol === "airplay" ? "AIRPLAY" : "MIRACAST"
}

// The portal decides whether to mirror an existing monitor or extend into the
// temporary output, so the connect request carries no display mode.
function connectRequest(receiver, testPattern) {
  return {
    command: "connect", protocol: receiver.protocol, mac: receiver.mac,
    ip: receiver.ip, port: receiver.port, name: receiver.name, test: !!testPattern
  }
}

function peers(value) {
  if (!Array.isArray(value)) return []
  var result = []
  var seen = ({})
  var macPattern = /^([0-9A-F]{2}:){5}[0-9A-F]{2}$/
  for (var i = 0; i < Math.min(value.length, 64); i++) {
    var peer = value[i]
    if (!peer) continue
    var protocol = peer.protocol === "airplay" ? "airplay" : "miracast"
    var mac = String(peer.mac || peer.device_id || "").toUpperCase()
    var ip = plain(peer.ip, 64)
    var port = Math.max(0, Math.min(65535, Number(peer.port) || 0))
    if (protocol === "miracast") {
      if (!peer.wfd || !macPattern.test(mac)) continue
    } else if (!ip || port === 0) {
      continue
    }
    if (!mac) mac = ip
    var key = protocol + ":" + mac
    if (seen[key]) continue
    seen[key] = true
    result.push({
      key: key,
      protocol: protocol,
      mac: mac,
      name: plain(peer.name, 120) || mac,
      model: plain(peer.model, 120),
      strength: Math.max(0, Math.min(100, Number(peer.strength) || 0)),
      ip: ip,
      port: port,
      wfd: peer.wfd === true
    })
  }
  result.sort(function (left, right) {
    if (left.protocol !== right.protocol) return left.protocol === "airplay" ? -1 : 1
    if (left.strength !== right.strength) return right.strength - left.strength
    return left.name.localeCompare(right.name)
  })
  return result
}

function bitrate(kbps) {
  var value = Number(kbps)
  return isFinite(value) && value > 0 ? (value / 1000).toFixed(2) + " Mb/s" : "—"
}
