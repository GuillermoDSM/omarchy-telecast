pragma Singleton

import QtQuick
import Quickshell
import Quickshell.Io
import "MiracastModel.js" as Model

// Shared by every bar surface, including the headless output we create. A
// second bar must NOT start a second backend or steal the active session.
QtObject {
  id: root

  property int users: 0
  property bool engineReady: false
  property var queued: []
  property string phase: "offline"
  property string detail: "AirPlay y Miracast experimentales · el audio AirPlay también suena en la laptop"
  property bool busy: false
  property var receivers: []
  property string selectedKey: ""
  property var checks: []
  property bool prerequisitesReady: false
  property bool airplayReady: false
  property bool pinRequired: false
  property string pinKind: "pin"
  property bool airplayAudio: false
  property bool testPattern: false
  property int kbps: 0
  property bool feedback: false
  property string resolution: ""
  property string warning: ""
  property var history: []
  property string logPath: ""

  readonly property string enginePath: decodeURIComponent(String(Qt.resolvedUrl("bin/omarchy-miracast")).replace(/^file:\/\//, ""))
  readonly property string statusLabel: Model.phaseLabel(root.phase)
  readonly property bool streaming: root.phase === "streaming"

  function attach() { root.users++ }
  function detach() {
    root.users = Math.max(0, root.users - 1)
    if (root.users === 0 && backend.running) backend.signal(15)
  }

  function appendHistory(message) {
    var next = root.history.slice(-39)
    next.push(new Date().toLocaleTimeString() + "  " + Model.plain(message, 900))
    root.history = next
  }

  function ensureEngine() {
    if (backend.running) return
    root.engineReady = false
    root.warning = ""
    backend.running = true
  }

  function send(command) {
    if (command.command === "stop") root.queued = []
    if (root.engineReady && backend.running) {
      backend.write(JSON.stringify(command) + "\n")
    } else {
      if (root.queued.length >= 8) return
      root.queued = root.queued.concat([command])
      root.ensureEngine()
    }
  }

  function discover() {
    if (root.busy) return
    root.warning = ""
    root.busy = true
    root.phase = "searching"
    root.send({command: "scan"})
  }

  function selectedReceiver() {
    for (var i = 0; i < root.receivers.length; i++) {
      if (root.receivers[i].key === root.selectedKey) return root.receivers[i]
    }
    return null
  }

  function connectSelected() {
    var receiver = root.selectedReceiver()
    if (root.busy || !receiver) return
    if (receiver.protocol === "airplay" ? !root.airplayReady : !root.prerequisitesReady) return
    root.kbps = 0
    root.feedback = false
    root.resolution = ""
    root.warning = ""
    root.pinRequired = false
    root.airplayAudio = false
    root.busy = true
    root.phase = receiver.protocol === "airplay" ? "connecting" : "starting"
    root.send({
      command: "connect",
      protocol: receiver.protocol,
      mac: receiver.mac,
      ip: receiver.ip,
      port: receiver.port,
      name: receiver.name,
      test: root.testPattern
    })
  }

  function submitPin(code) {
    var value = String(code || "").replace(/[\x00-\x1f\x7f]/g, "").slice(0, 128)
    if (!value) return
    root.send({command: "pin", pin: value})
  }

  function stop() {
    if (!backend.running) return
    root.send({command: "stop"})
  }

  function receive(line) {
    try {
      var event = JSON.parse(line)
      if (event.event === "ready") {
        root.engineReady = true
        // Diagnostics never scan or activate a network connection.
        backend.write(JSON.stringify({command: "doctor"}) + "\n")
        var pending = root.queued
        root.queued = []
        for (var i = 0; i < pending.length; i++) backend.write(JSON.stringify(pending[i]) + "\n")
      } else if (event.event === "state") {
        root.phase = String(event.phase || "error")
        root.detail = Model.plain(event.detail, 1200)
        root.busy = Boolean(event.busy)
        if (root.phase !== "pairing") root.pinRequired = false
        root.appendHistory(root.detail)
      } else if (event.event === "peers") {
        root.receivers = Model.peers(event.peers)
        if (!root.receivers.some(function (p) { return p.key === root.selectedKey }))
          root.selectedKey = root.receivers.length ? root.receivers[0].key : ""
      } else if (event.event === "diagnostics") {
        root.checks = Array.isArray(event.checks) ? event.checks.slice(0, 40) : []
        root.prerequisitesReady = event.ready === true
        root.airplayReady = event.airplay_ready === true
      } else if (event.event === "pin_required") {
        root.pinRequired = true
        root.pinKind = String(event.kind || "pin")
      } else if (event.event === "airplay") {
        root.airplayAudio = event.audio === true
        root.feedback = false
        root.resolution = "AirPlay"
      } else if (event.event === "metrics") {
        root.kbps = Math.max(0, Math.min(8000, Number(event.kbps) || 0))
        root.feedback = event.feedback === true
        root.resolution = String(event.width) + "×" + String(event.height) + " · " + String(event.fps) + " fps"
      } else if (event.event === "warning") {
        root.warning = Model.plain(event.detail, 1200)
        root.appendHistory(root.warning)
      } else if (event.event === "log") {
        root.logPath = Model.plain(event.path, 1024)
      }
    } catch (error) {
      root.warning = "Respuesta inválida del motor; consulta el diagnóstico"
    }
  }

  property Process backend: Process {
    command: [root.enginePath, "serve"]
    stdinEnabled: true
    stdout: SplitParser { onRead: function (line) { root.receive(line) } }
    stderr: SplitParser {
      onRead: function (line) { root.appendHistory("Motor: " + line) }
    }
    onExited: function (exitCode, exitStatus) {
      root.engineReady = false
      root.busy = false
      root.queued = []
      root.pinRequired = false
      root.phase = exitCode === 0 ? "offline" : "error"
      root.detail = exitCode === 0 ? "Motor detenido" : "El motor terminó con código " + exitCode + ". Abre Diagnóstico para ver detalles."
    }
  }

  property IpcHandler ipc: IpcHandler {
    target: "astro67.miracast"
    function scan(): string { root.discover(); return "ok" }
    function stop(): string { root.stop(); return "ok" }
    function status(): string {
      return JSON.stringify({phase: root.phase, detail: root.detail, busy: root.busy,
        receivers: root.receivers, selectedKey: root.selectedKey, pinRequired: root.pinRequired,
        kbps: root.kbps, feedback: root.feedback,
        warning: root.warning, logPath: root.logPath})
    }
  }
}
