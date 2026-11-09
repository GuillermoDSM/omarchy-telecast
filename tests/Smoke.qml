import QtQuick
import QtQuick.Window
import Quickshell

ShellRoot {
  id: root
  property var first: null
  property var second: null
  property var service: null
  Window { id: window; visible: false }

  function waitForUsers(expected, failure, callback) {
    waitTimer.expected = expected
    waitTimer.failure = failure
    waitTimer.callback = callback
    waitTimer.attempts = 0
    waitTimer.start()
  }

  Timer {
    id: waitTimer
    interval: 20
    repeat: true
    running: false
    property int expected: 0
    property int attempts: 0
    property string failure: ""
    property var callback: null
    onTriggered: {
      if (root.service.users === expected) {
        stop()
        var next = callback
        callback = null
        if (next) next()
      } else if (++attempts > 50) {
        stop()
        console.error("SMOKE FAILED: " + failure)
        Qt.exit(1)
      }
    }
  }

  Timer {
    interval: 50
    running: true
    onTriggered: {
      var source = Quickshell.env("CAST_SOURCE")
      var component = Qt.createComponent("file://" + source + "/TelecastWidget.qml")
      if (component.status !== Component.Ready) {
        console.error("SMOKE FAILED: " + component.errorString())
        Qt.exit(1)
        return
      }
      root.first = component.createObject(window.contentItem)
      root.second = component.createObject(window.contentItem)
      if (!root.first || !root.second || root.first.service !== root.second.service) {
        console.error("SMOKE FAILED: widgets do not share a service")
        Qt.exit(1)
        return
      }
      root.service = root.first.service
      if (root.service.users !== 2) {
        console.error("SMOKE FAILED: widget lifecycle accounting")
        Qt.exit(1)
        return
      }
      root.service.receive(JSON.stringify({event: "peers", peers: [
        {mac: "68:72:C3:A8:40:DF", name: "Samsung <TV>", model: "UTU7000", ip: "192.168.1.139", port: 7000}
      ]}))
      root.service.receive(JSON.stringify({event: "state", phase: "streaming", busy: true, detail: "Simulated UI state"}))
      if (root.service.receivers.length !== 1 || !root.service.streaming) {
        console.error("SMOKE FAILED: event delivery")
        Qt.exit(1)
        return
      }
      root.first.destroy()
      root.waitForUsers(1, "duplicate monitor cleanup", function () {
        root.second.destroy()
        root.waitForUsers(0, "final widget cleanup", function () {
          console.log("SMOKE PASSED: QML loads, shared service, events, widget lifecycle")
          Qt.quit()
        })
      })
    }
  }
}
