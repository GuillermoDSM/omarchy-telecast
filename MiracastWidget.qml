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
    if (root.popupOpen && !root.controller.busy) root.controller.discover()
  }

  function activateReceiver(key) {
    if (root.controller.busy) {
      if (root.controller.selectedKey === key && root.controller.phase !== "searching") root.controller.stop()
      return
    }
    var receiver = null
    for (var i = 0; i < root.controller.receivers.length; i++) {
      if (root.controller.receivers[i].key === key) receiver = root.controller.receivers[i]
    }
    if (!receiver) return
    if (receiver.protocol === "airplay" ? !root.controller.airplayReady : !root.controller.prerequisitesReady) return
    root.controller.selectedKey = key
    root.close()
    root.controller.connectSelected()
  }

  Component.onCompleted: root.controller.attach()
  Component.onDestruction: root.controller.detach()

  Ui.WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.controller.streaming && !root.vertical ? "󰄘 " + Model.bitrate(root.controller.kbps) : "󰄘"
    tooltipText: "Enviar pantalla · " + root.controller.statusLabel + "\n" + root.controller.detail
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
    focusTarget: root.controller.pinRequired ? pinInput : keys
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
          text: "ENVIAR PANTALLA"
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
          text: "AirPlay para Samsung/LG/Sony compatibles · la TV aparecerá como salida de audio · Miracast como respaldo"
          textFormat: Text.PlainText
          wrapMode: Text.Wrap
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
            text: root.controller.phase === "searching" ? "Cancelar búsqueda" : "Cancelar conexión"
            visible: root.controller.busy && !root.controller.streaming
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
          text: "Pon la TV en Compartir pantalla. Selecciona una TV para conectar."
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
            color: root.controller.selectedKey === modelData.key ? Qt.rgba(Commons.Color.accent.r, Commons.Color.accent.g, Commons.Color.accent.b, 0.14) : "transparent"
            border.width: root.controller.selectedKey === modelData.key ? 1 : 0
            border.color: Commons.Color.accent
            activeFocusOnTab: true
            Accessible.role: Accessible.Button
            Accessible.name: modelData.name + (root.controller.busy && root.controller.selectedKey === modelData.key ? " · Desconectar" : " · Conectar")
            Keys.onReturnPressed: root.activateReceiver(modelData.key)
            Keys.onSpacePressed: root.activateReceiver(modelData.key)
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
                text: root.controller.busy && root.controller.selectedKey === receiverRow.modelData.key && root.controller.phase !== "searching"
                  ? "Desconectar"
                  : Model.protocolLabel(receiverRow.modelData.protocol) + (receiverRow.modelData.protocol === "airplay"
                    ? (receiverRow.modelData.model ? " · " + receiverRow.modelData.model : " · Conectar")
                    : " · " + receiverRow.modelData.strength + "%")
                textFormat: Text.PlainText
                color: Qt.darker(root.foreground, 1.4)
                font.family: root.fontFamily
                font.pixelSize: Style.font.caption
              }
            }
            MouseArea {
              anchors.fill: parent
              enabled: (!root.controller.busy && (receiverRow.modelData.protocol === "airplay" ? root.controller.airplayReady : root.controller.prerequisitesReady))
                || (root.controller.busy && root.controller.selectedKey === receiverRow.modelData.key && root.controller.phase !== "searching")
              cursorShape: Qt.PointingHandCursor
              onClicked: { receiverRow.forceActiveFocus(); root.activateReceiver(receiverRow.modelData.key) }
            }
          }
        }

        Column {
          width: parent.width
          spacing: Style.space(6)
          visible: root.controller.pinRequired
          onVisibleChanged: if (visible) Qt.callLater(pinInput.forceActiveFocus)
          Text {
            width: parent.width
            text: root.controller.pinKind === "password" ? "Contraseña de la TV" : "PIN que muestra la TV"
            textFormat: Text.PlainText
            color: Commons.Color.accent
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
            font.bold: true
          }
          Ui.TextField {
            id: pinInput
            width: parent.width
            placeholderText: root.controller.pinKind === "password" ? "Contraseña" : "PIN"
            inputMethodHints: root.controller.pinKind === "password" ? Qt.ImhNone : Qt.ImhDigitsOnly
            maximumLength: 128
            foreground: root.foreground
            onAccepted: {
              root.controller.submitPin(text)
              text = ""
            }
          }
          Ui.Button {
            width: parent.width
            text: "Enviar código"
            bordered: true
            foreground: root.foreground
            onClicked: {
              root.controller.submitPin(pinInput.text)
              pinInput.text = ""
            }
          }
        }

        Ui.Button {
          width: parent.width
          visible: root.showDiagnostics
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
          text: root.controller.resolution === "AirPlay"
            ? "AirPlay · " + (root.controller.airplayAudio
                ? (root.controller.airplayOutput
                  ? "salida de audio de la TV disponible en el panel de Omarchy"
                  : "audio no disponible en la salida de la TV")
                : "sin audio") + "\n"
              + "Puedes cambiar entre la TV y el altavoz en el panel de audio, como cualquier otra salida. El volumen de la TV no se modifica"
            : root.controller.resolution + " · " + Model.bitrate(root.controller.kbps) + "\n"
              + (root.controller.feedback ? "Adaptación por informes RTCP de la TV" : "Sin RTCP: recuperación local limitada; recepción no verificada")
          textFormat: Text.PlainText
          wrapMode: Text.Wrap
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        Text {
          width: parent.width
          text: "Experimental. AirPlay no cambia el volumen de la TV; Miracast puede afectar al Wi-Fi habitual y el motor cancela si detecta un cambio. No modifica UFW."
          textFormat: Text.PlainText
          wrapMode: Text.Wrap
          color: Qt.darker(root.foreground, 1.4)
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
        }

        Ui.Button {
          width: parent.width
          text: root.showDiagnostics ? "Ocultar depuración" : "Depuración"
          bordered: true
          foreground: root.foreground
          onClicked: {
            root.showDiagnostics = !root.showDiagnostics
            if (!root.showDiagnostics) root.controller.testPattern = false
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
