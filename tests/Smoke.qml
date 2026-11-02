import QtQuick
import QtQuick.Window
import Quickshell

ShellRoot {
  id: root
  property var first: null
  property var second: null
  property var controller: null
  Window { id: window; visible: false }

  Timer {
    interval: 50
    running: true
    onTriggered: {
      var source = Quickshell.env("MIRACAST_SOURCE")
      var component = Qt.createComponent("file://" + source + "/MiracastWidget.qml")
      if (component.status !== Component.Ready) {
        console.error("SMOKE FAILED: " + component.errorString())
        Qt.exit(1)
        return
      }
      root.first = component.createObject(window.contentItem)
      root.second = component.createObject(window.contentItem)
      if (!root.first || !root.second || root.first.controller !== root.second.controller) {
        console.error("SMOKE FAILED: widgets do not share a controller")
        Qt.exit(1)
        return
      }
      root.controller = root.first.controller
      if (root.controller.users !== 2) {
        console.error("SMOKE FAILED: widget lifecycle accounting")
        Qt.exit(1)
        return
      }
      root.controller.receive(JSON.stringify({event: "peers", peers: [
        {mac: "BE:7E:8B:BE:29:2E", name: "Samsung <TV>", wfd: true, strength: 70}
      ]}))
      root.controller.receive(JSON.stringify({event: "state", phase: "streaming", busy: true, detail: "Simulated UI state"}))
      root.controller.receive(JSON.stringify({event: "metrics", kbps: 3750, feedback: true, width: 1920, height: 1080, fps: 30}))
      if (root.controller.receivers.length !== 1 || root.controller.kbps !== 3750 || !root.controller.streaming) {
        console.error("SMOKE FAILED: event delivery")
        Qt.exit(1)
        return
      }
      root.first.destroy()
      Qt.callLater(function () {
        if (root.controller.users !== 1) {
          console.error("SMOKE FAILED: duplicate monitor cleanup")
          Qt.exit(1)
          return
        }
        root.second.destroy()
        Qt.callLater(function () {
          if (root.controller.users !== 0) {
            console.error("SMOKE FAILED: final widget cleanup")
            Qt.exit(1)
            return
          }
          console.log("SMOKE PASSED: QML loads, shared controller, events, widget lifecycle")
          Qt.quit()
        })
      })
    }
  }
}
