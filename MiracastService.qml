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
  property string detail: "Miracast experimental · escritorio extendido hasta 1080p"
  property bool busy: false
  property var receivers: []
  property string selectedMac: ""
  property var checks: []
  property bool prerequisitesReady: false
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

  function connectSelected() {
    if (root.busy || !root.selectedMac || !root.prerequisitesReady) return
    root.kbps = 0
    root.feedback = false
    root.resolution = ""
    root.warning = ""
    root.busy = true
    root.phase = "starting"
    root.send({command: "connect", mac: root.selectedMac, test: root.testPattern})
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
        root.appendHistory(root.detail)
      } else if (event.event === "peers") {
        root.receivers = Model.peers(event.peers)
        if (!root.receivers.some(function (p) { return p.mac === root.selectedMac }))
          root.selectedMac = root.receivers.length ? root.receivers[0].mac : ""
      } else if (event.event === "diagnostics") {
        root.checks = Array.isArray(event.checks) ? event.checks.slice(0, 32) : []
        root.prerequisitesReady = event.ready === true
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
        receivers: root.receivers, kbps: root.kbps, feedback: root.feedback, warning: root.warning, logPath: root.logPath})
    }
  }
}
