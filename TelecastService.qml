pragma Singleton

import QtQuick
import Quickshell
import Quickshell.Io
import "TelecastModel.js" as Model

// Shared by every bar surface, including the headless output we create. A
// second bar must NOT start a second backend or steal the active session.
QtObject {
  id: root

  property int users: 0
  property bool engineReady: false
  // The power switch is independent of whether a scan or session is active.
  property bool enabled: true
  property bool shuttingDown: false
  property var queued: []
  property string phase: "offline"
  property string detail: "AirPlay experimental"
  property bool busy: false
  property bool discovering: false
  property var receivers: []
  property string selectedKey: ""
  property var checks: []
  property bool airplayReady: false
  property bool pinRequired: false
  property string pinKind: "pin"
  property bool testPattern: false
  property var activeReceiver: null
  property string warning: ""
  property var history: []
  property string logPath: ""

  readonly property string enginePath: decodeURIComponent(String(Qt.resolvedUrl("bin/omarchy-telecast")).replace(/^file:\/\//, ""))
  readonly property string uiLanguage: {
    var override = String(Quickshell.env("TELECAST_LANG") || "").toLowerCase()
    if (override === "es" || override === "en") return override
    return Model.language(Qt.locale().name)
  }
  readonly property var i18n: Model.dictionary(root.uiLanguage)
  readonly property string statusLabel: Model.phaseLabel(root.phase, root.uiLanguage)
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
    if (!root.enabled && command.command !== "quit") return
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
    if (!root.enabled || root.shuttingDown || root.busy || root.discovering) return
    root.warning = ""
    root.receivers = []
    root.selectedKey = ""
    root.discovering = true
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
    if (!root.enabled || root.shuttingDown || root.busy || !receiver || !root.airplayReady) return
    root.warning = ""
    root.pinRequired = false
    root.discovering = false
    root.busy = true
    root.activeReceiver = receiver
    root.phase = "connecting"
    root.send(Model.connectRequest(receiver, root.testPattern))
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

  function setEnabled(on) {
    if (root.enabled === on) return
    root.enabled = on
    root.receivers = []
    root.selectedKey = ""
    root.queued = []
    root.pinRequired = false
    root.warning = ""
    if (!on) {
      root.discovering = false
      root.busy = false
      root.phase = "offline"
      root.detail = root.i18n.screenDisabled
      if (backend.running) {
        root.shuttingDown = true
        // Quit follows the engine's cooperative session cleanup.
        backend.write(JSON.stringify({command: "quit"}) + "\n")
      }
    } else if (!root.shuttingDown) {
      root.discover()
    } else {
      root.phase = "offline"
      root.detail = root.i18n.waitingDisconnect
    }
  }

  function receive(line) {
    try {
      var event = JSON.parse(line)
      // Ignore late discovery/session events while the old engine shuts down.
      if (root.shuttingDown || !root.enabled) return
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
        root.discovering = event.discovering === true || root.phase === "searching"
        if (root.phase === "idle" || root.phase === "error") root.activeReceiver = null
        if (root.phase !== "pairing") root.pinRequired = false
        root.appendHistory(root.detail)
      } else if (event.event === "peers") {
        root.receivers = Model.peers(event.peers)
        if (!root.receivers.some(function (p) { return p.key === root.selectedKey }))
          root.selectedKey = root.receivers.length ? root.receivers[0].key : ""
      } else if (event.event === "diagnostics") {
        root.checks = Array.isArray(event.checks) ? event.checks.slice(0, 40) : []
        root.airplayReady = event.ready === true
      } else if (event.event === "pin_required") {
        root.pinRequired = true
        root.pinKind = String(event.kind || "pin")
      } else if (event.event === "warning") {
        root.warning = Model.plain(event.detail, 1200)
        root.appendHistory(root.warning)
      } else if (event.event === "log") {
        root.logPath = Model.plain(event.path, 1024)
      }
    } catch (error) {
      root.warning = root.i18n.invalidResponse
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
      var intentional = root.shuttingDown
      root.shuttingDown = false
      root.engineReady = false
      root.busy = false
      root.discovering = false
      root.queued = []
      root.pinRequired = false
      root.activeReceiver = null
      root.receivers = []
      root.selectedKey = ""
      if (intentional) {
        root.phase = "offline"
        root.detail = root.enabled ? root.i18n.engineStopped : root.i18n.screenDisabled
        if (root.enabled) Qt.callLater(root.discover)
        return
      }
      root.phase = exitCode === 0 ? "offline" : "error"
      root.detail = exitCode === 0
        ? root.i18n.engineStopped
        : root.i18n.engineExit + exitCode + root.i18n.engineExitHint
    }
  }

  property IpcHandler ipc: IpcHandler {
    target: "astro67.telecast"
    function scan(): string { root.discover(); return "ok" }
    function stop(): string { root.stop(); return "ok" }
    function enable(): string { root.setEnabled(true); return "ok" }
    function disable(): string { root.setEnabled(false); return "ok" }
    function status(): string {
      return JSON.stringify({phase: root.phase, detail: root.detail, busy: root.busy,
         enabled: root.enabled, discovering: root.discovering, receivers: root.receivers,
        selectedKey: root.selectedKey, pinRequired: root.pinRequired,
        warning: root.warning, logPath: root.logPath})
    }
  }
}
