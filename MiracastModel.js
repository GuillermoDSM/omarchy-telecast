.pragma library

function phaseLabel(phase) {
  var labels = {
    offline: "ENVIAR PANTALLA", idle: "LISTO", searching: "BUSCANDO",
    display: "MONITOR VIRTUAL", capture: "ELIGE PANTALLA",
    connecting: "CONECTANDO", pairing: "EMPAREJANDO",
    streaming: "ENVIANDO PANTALLA", stopping: "DESCONECTANDO", error: "ERROR"
  }
  return labels[phase] || "ENVIAR PANTALLA"
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
