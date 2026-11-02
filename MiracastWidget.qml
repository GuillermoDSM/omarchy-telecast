import QtQuick
import Quickshell
import qs.Commons
import qs.Commons as Commons
import qs.Ui as Ui
import "." as Cast
import "MiracastModel.js" as Model

Ui.BarWidget {
  id: root
  moduleName: "astro67.miracast"

  property bool popupOpen: false
  property bool showDiagnostics: false
  readonly property var controller: Cast.MiracastService
  readonly property color foreground: root.bar ? root.bar.barForeground : Commons.Color.foreground
  readonly property string fontFamily: root.bar ? root.bar.fontFamily : Style.font.family

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  function close() { root.popupOpen = false }
  function toggle() {
    root.popupOpen = !root.popupOpen
    if (root.popupOpen) root.controller.ensureEngine()
  }

  Component.onCompleted: root.controller.attach()
  Component.onDestruction: root.controller.detach()

  Ui.WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.controller.streaming && !root.vertical ? "󰄘 " + Model.bitrate(root.controller.kbps) : "󰄘"
    tooltipText: "Miracast · " + root.controller.statusLabel + "\n" + root.controller.detail
    foreground: root.controller.phase === "error" ? Commons.Color.urgent : root.controller.streaming ? Commons.Color.accent : root.foreground
    onPressed: function (mouseButton) {
      if (mouseButton === Qt.MiddleButton) root.controller.stop()
      else root.toggle()
    }
  }

  Ui.KeyboardPanel {
    id: popup
    anchorItem: button
    bar: root.bar
    owner: root
    open: root.popupOpen
    focusTarget: keys
    contentWidth: popup.fittedContentWidth(Style.space(360))
    contentHeight: popup.fittedContentHeight(content.implicitHeight, Style.space(740))

    Ui.PanelKeyCatcher {
      id: keys
      anchors.fill: parent
      onCloseRequested: root.close()
    }

    Flickable {
      anchors.fill: parent
      contentHeight: content.implicitHeight
      clip: true
      boundsBehavior: Flickable.StopAtBounds

      Column {
        id: content
        width: parent.width
        spacing: Style.space(10)

        Text {
          width: parent.width
          text: "MIRACAST"
          textFormat: Text.PlainText
          font.family: root.fontFamily
          font.pixelSize: Style.font.heading
          font.bold: true
          color: root.foreground
        }

        Text {
          width: parent.width
          text: root.controller.statusLabel
          textFormat: Text.PlainText
          color: root.controller.phase === "error" ? Commons.Color.urgent : Commons.Color.accent
          font.family: root.fontFamily
          font.pixelSize: Style.font.body
          font.bold: true
        }

        Text {
          width: parent.width
          text: root.controller.detail
          textFormat: Text.PlainText
          wrapMode: Text.Wrap
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        Text {
          width: parent.width
          visible: root.controller.warning !== ""
          text: root.controller.warning
          textFormat: Text.PlainText
          wrapMode: Text.Wrap
          color: Commons.Color.urgent
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        Ui.PanelSeparator { foreground: root.foreground }

        Text {
          width: parent.width
          text: "Extender · máximo 1080p · sin audio"
          textFormat: Text.PlainText
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        Row {
          width: parent.width
          spacing: Style.space(8)
          Ui.Button {
            width: (parent.width - parent.spacing) / 2
            text: "Buscar TV"
            bordered: true
            enabled: !root.controller.busy
            foreground: root.foreground
            onClicked: root.controller.discover()
          }
          Ui.Button {
            width: (parent.width - parent.spacing) / 2
            text: "Detener"
            bordered: true
            enabled: root.controller.busy
            foreground: root.foreground
            accent: Commons.Color.urgent
            onClicked: root.controller.stop()
          }
        }

        Text {
          visible: root.controller.receivers.length === 0
          width: parent.width
          text: "Pon la TV en Compartir pantalla y pulsa Buscar TV. No se conecta automáticamente."
          textFormat: Text.PlainText
          wrapMode: Text.Wrap
          color: Qt.darker(root.foreground, 1.4)
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        Repeater {
          model: root.controller.receivers
          delegate: Rectangle {
            id: receiverRow
            required property var modelData
            width: content.width
            height: receiverText.implicitHeight + Style.space(18)
            radius: Style.space(3)
            color: root.controller.selectedMac === modelData.mac ? Qt.rgba(Commons.Color.accent.r, Commons.Color.accent.g, Commons.Color.accent.b, 0.14) : "transparent"
            border.width: root.controller.selectedMac === modelData.mac ? 1 : 0
            border.color: Commons.Color.accent
            activeFocusOnTab: true
            Accessible.role: Accessible.Button
            Accessible.name: modelData.name
            Keys.onReturnPressed: root.controller.selectedMac = modelData.mac
            Column {
              id: receiverText
              anchors.centerIn: parent
              width: parent.width - Style.space(18)
              spacing: Style.space(3)
              Text {
                width: parent.width
                text: receiverRow.modelData.name
                textFormat: Text.PlainText
                elide: Text.ElideRight
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.body
              }
              Text {
                width: parent.width
                text: receiverRow.modelData.mac + " · " + receiverRow.modelData.strength + "%"
                textFormat: Text.PlainText
                color: Qt.darker(root.foreground, 1.4)
                font.family: root.fontFamily
                font.pixelSize: Style.font.caption
              }
            }
            MouseArea {
              anchors.fill: parent
              enabled: !root.controller.busy
              cursorShape: Qt.PointingHandCursor
              onClicked: { root.controller.selectedMac = receiverRow.modelData.mac; receiverRow.forceActiveFocus() }
            }
          }
        }

        Ui.Button {
          width: parent.width
          text: root.controller.testPattern ? "Conectar patrón de prueba" : "Extender escritorio"
          bordered: true
          foreground: root.foreground
          accent: Commons.Color.accent
          enabled: !root.controller.busy && root.controller.selectedMac !== "" && root.controller.prerequisitesReady
          onClicked: {
            // Release the keyboard grab before the portal picker opens.
            root.close()
            root.controller.connectSelected()
          }
        }

        Ui.Button {
          width: parent.width
          text: root.controller.testPattern ? "✓ Patrón de prueba (sin capturar pantalla)" : "Patrón de prueba (sin capturar pantalla)"
          selected: root.controller.testPattern
          bordered: true
          foreground: root.foreground
          enabled: !root.controller.busy
          onClicked: root.controller.testPattern = !root.controller.testPattern
        }

        Text {
          width: parent.width
          visible: root.controller.streaming
          text: root.controller.resolution + " · " + Model.bitrate(root.controller.kbps) + "\n"
            + (root.controller.feedback ? "Adaptación por informes RTCP de la TV" : "Sin RTCP: recuperación local limitada; recepción no verificada")
          textFormat: Text.PlainText
          wrapMode: Text.Wrap
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        Ui.Button {
          width: parent.width
          text: "Pantallas de Omarchy"
          bordered: true
          foreground: root.foreground
          onClicked: {
            root.close()
            Quickshell.execDetached(["omarchy-shell", "shell", "toggle", "omarchy.monitor", "{}"])
          }
        }

        Text {
          width: parent.width
          text: "Experimental. Wi-Fi Direct puede afectar al Wi-Fi habitual; el motor cancela si detecta un cambio. No modifica UFW."
          textFormat: Text.PlainText
          wrapMode: Text.Wrap
          color: Qt.darker(root.foreground, 1.4)
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }

        Ui.Button {
          width: parent.width
          text: root.showDiagnostics ? "Ocultar diagnóstico" : "Diagnóstico"
          bordered: true
          foreground: root.foreground
          onClicked: {
            root.showDiagnostics = !root.showDiagnostics
            if (root.showDiagnostics && !root.controller.busy) root.controller.send({command: "doctor"})
          }
        }

        Column {
          width: parent.width
          spacing: Style.space(5)
          visible: root.showDiagnostics
          Text {
            width: parent.width
            visible: root.controller.logPath !== ""
            text: "Registro: " + root.controller.logPath
            textFormat: Text.PlainText
            wrapMode: Text.WrapAnywhere
            color: root.foreground
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
          }
          Repeater {
            model: root.controller.checks
            delegate: Text {
              required property var modelData
              width: content.width
              text: (modelData.ok ? "✓ " : "✗ ") + modelData.name + ": " + modelData.detail
              textFormat: Text.PlainText
              wrapMode: Text.Wrap
              color: modelData.ok ? root.foreground : Commons.Color.urgent
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
            }
          }
          Text {
            width: parent.width
            text: root.controller.history.join("\n")
            textFormat: Text.PlainText
            wrapMode: Text.WrapAnywhere
            color: Qt.darker(root.foreground, 1.3)
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
          }
        }
      }
    }
  }
}
