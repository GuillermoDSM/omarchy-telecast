import QtQuick
import QtQuick.Controls
import Quickshell
import qs.Ui
import qs.Ui as Ui
import qs.Commons
import qs.Commons as Commons
import "." as Cast
import "MiracastModel.js" as Model

Panel {
  id: root
  moduleName: "astro67.miracast"
  ipcTarget: "astro67.miracast"
  // The MiracastService singleton owns the shared IPC handler for scan/stop/
  // status. The panel only needs the base class's open/close lifecycle.
  manageIpc: false

  readonly property var service: Cast.MiracastService
  readonly property color foreground: root.barForeground
  readonly property string fontFamily: root.bar ? root.bar.fontFamily : Style.font.family
  readonly property bool active: service.enabled

  property bool showDiagnostics: false
  property int phraseIndex: 0
  property string focusSection: "header"
  property int selectedIndex: 0
  property bool cursorActive: false
  property string focusedReceiverKey: ""

  readonly property var activePhrases: [
    "Buscando señales",
    "Sincronizando pantallas",
    "Enviando píxeles",
    "Ajustando latencia",
    "Tejiendo paquetes",
    "Domando radios",
    "Puliendo códecs",
    "Conectando mundos"
  ]
  readonly property bool rotatingPhrases: service.discovering
  readonly property string heroStatusText: {
    if (!service.enabled) return "Desactivado"
    if (service.shuttingDown) return "Desconectando"
    if (service.phase === "error") return "Error"
    if (service.discovering) return activePhrases[phraseIndex % activePhrases.length]
    if (service.busy) return service.statusLabel
    return service.receivers.length > 0 ? "Listo · elige una TV" : "Listo"
  }

  readonly property string barIcon: active ? "󰄙" : "󰄘"

  readonly property var selectedReceiver: {
    for (var i = 0; i < service.receivers.length; i++) {
      if (service.receivers[i].key === service.selectedKey) return service.receivers[i]
    }
    return null
  }
  readonly property var connectedReceiver: service.busy ? selectedReceiver : null
  readonly property var availableReceivers: service.busy ? [] : service.receivers

  readonly property var visibleSections: {
    var sections = []
    if (connectedReceiver) sections.push("connected")
    if (availableReceivers.length > 0) sections.push("available")
    return sections
  }
  readonly property int scrollRowIndex: focusSection === "available" ? selectedIndex : -1
  readonly property bool headerHasCursor: cursorActive && focusSection === "header"

  readonly property color hoverFill: root.bar
    ? Style.hoverFillFor(root.foreground, Commons.Color.accent)
    : "transparent"
  readonly property color selectedFill: root.bar
    ? Style.selectedFillFor(root.foreground, Commons.Color.accent)
    : "transparent"

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  function toggleActive() {
    service.setEnabled(!service.enabled)
  }

  function activateReceiver(key) {
    if (service.busy) {
      if (service.selectedKey === key) service.stop()
      return
    }
    var receiver = null
    for (var i = 0; i < service.receivers.length; i++) {
      if (service.receivers[i].key === key) receiver = service.receivers[i]
    }
    if (!receiver) return
    if (receiver.protocol === "airplay" ? !service.airplayReady : !service.prerequisitesReady) return
    service.selectedKey = key
    root.close()
    service.connectSelected()
  }

  function toggleDiagnostics() {
    showDiagnostics = !showDiagnostics
    if (!showDiagnostics) service.testPattern = false
    if (showDiagnostics && service.enabled && !service.busy && !service.discovering)
      service.send({command: "doctor"})
  }

  function sectionCount(section) {
    if (section === "connected") return connectedReceiver ? 1 : 0
    if (section === "available") return availableReceivers.length
    return 0
  }

  function sectionDevices(section) {
    if (section === "connected") return connectedReceiver ? [connectedReceiver] : []
    if (section === "available") return availableReceivers
    return []
  }

  function receiverAt(section, index) {
    var list = sectionDevices(section)
    return index >= 0 && index < list.length ? list[index] : null
  }

  function moveCursor(delta) {
    cursorActive = true
    var sections = visibleSections
    if (focusSection === "header") {
      if (delta > 0 && sections.length > 0) {
        focusSection = sections[0]
        selectedIndex = 0
      }
      return
    }
    if (!sections || sections.length === 0) {
      focusSection = "header"
      selectedIndex = 0
      return
    }
    var sIdx = sections.indexOf(focusSection)
    if (sIdx < 0) {
      focusSection = sections[0]
      selectedIndex = 0
      return
    }
    var count = sectionCount(focusSection)
    if (delta > 0) {
      if (selectedIndex < count - 1) selectedIndex += 1
      else if (sIdx < sections.length - 1) {
        focusSection = sections[sIdx + 1]
        selectedIndex = 0
      }
    } else {
      if (selectedIndex > 0) selectedIndex -= 1
      else if (sIdx > 0) {
        focusSection = sections[sIdx - 1]
        selectedIndex = Math.max(0, sectionCount(sections[sIdx - 1]) - 1)
      } else {
        focusSection = "header"
        selectedIndex = 0
      }
    }
  }

  function setHeaderCursor() {
    cursorActive = true
    focusSection = "header"
    selectedIndex = 0
  }

  function activateCursor() {
    if (focusSection === "header") {
      toggleActive()
      return
    }
    var receiver = receiverAt(focusSection, selectedIndex)
    if (receiver) activateReceiver(receiver.key)
  }

  function clampCursor() {
    var sections = visibleSections
    if (focusSection === "header") return
    if (!sections || sections.length === 0) {
      focusSection = "header"
      selectedIndex = 0
      return
    }
    if (sections.indexOf(focusSection) < 0) {
      focusSection = sections[0]
      selectedIndex = 0
      return
    }
    var count = sectionCount(focusSection)
    if (count === 0) {
      focusSection = "header"
      selectedIndex = 0
      return
    }
    if (selectedIndex > count - 1) selectedIndex = count - 1
    if (selectedIndex < 0) selectedIndex = 0
  }

  function updateFocusedKey() {
    var receiver = receiverAt(focusSection, selectedIndex)
    focusedReceiverKey = receiver ? receiver.key : ""
  }

  function reselectFocusedReceiver() {
    if (focusedReceiverKey === "") {
      clampCursor()
      return
    }
    var sections = ["connected", "available"]
    for (var s = 0; s < sections.length; s++) {
      var list = sectionDevices(sections[s])
      for (var i = 0; i < list.length; i++) {
        if (list[i].key === focusedReceiverKey) {
          focusSection = sections[s]
          selectedIndex = i
          clampCursor()
          return
        }
      }
    }
    clampCursor()
  }

  onSelectedIndexChanged: updateFocusedKey()
  onFocusSectionChanged: updateFocusedKey()
  onConnectedReceiverChanged: reselectFocusedReceiver()
  onAvailableReceiversChanged: reselectFocusedReceiver()
  onVisibleSectionsChanged: clampCursor()

  onOpenedChanged: {
    if (!opened) return
    if (service.enabled && !service.busy && !service.discovering) service.discover()
    if (service.busy) {
      focusSection = "connected"
      selectedIndex = 0
    } else if (service.receivers.length > 0) {
      focusSection = "available"
      selectedIndex = 0
    } else {
      focusSection = "header"
      selectedIndex = 0
    }
    cursorActive = false
  }

  Component.onCompleted: service.attach()
  Component.onDestruction: service.detach()

  Timer {
    id: phraseTimer
    interval: 2800
    running: root.opened && root.rotatingPhrases
    repeat: true
    onTriggered: phraseSwap.restart()
  }

  SequentialAnimation {
    id: phraseSwap
    PropertyAnimation {
      target: heroStatus
      property: "opacity"
      to: 0.0
      duration: 180
      easing.type: Easing.OutQuad
    }
    ScriptAction {
      script: root.phraseIndex = (root.phraseIndex + 1) % root.activePhrases.length
    }
    PropertyAnimation {
      target: heroStatus
      property: "opacity"
      to: 1.0
      duration: 260
      easing.type: Easing.InQuad
    }
  }

  Connections {
    target: root
    function onRotatingPhrasesChanged() {
      if (!root.rotatingPhrases) {
        phraseSwap.stop()
        heroStatus.opacity = 1.0
      }
    }
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.barIcon
    tooltipText: "Enviar pantalla · " + root.service.statusLabel
      + (root.service.streaming ? " · " + Model.bitrate(root.service.kbps) : "")
      + "\n" + root.service.detail
    foreground: root.service.phase === "error" ? Commons.Color.urgent : root.barForeground
    active: root.active
    useActiveColor: true
    activeColor: Commons.Color.accent
    onPressed: function(mouseButton) {
      if (mouseButton === Qt.MiddleButton) root.service.setEnabled(false)
      else if (mouseButton === Qt.RightButton) root.toggleActive()
      else root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: root.service.pinRequired ? pinInput : keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(380))
    contentHeight: panel.fittedContentHeight(column.implicitHeight, Style.space(740))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      blocked: root.service.pinRequired
      onMoveRequested: function(dx, dy) {
        if (dy !== 0) root.moveCursor(dy)
        else if (dx !== 0) root.cursorActive = true
      }
      onActivateRequested: if (root.cursorActive) root.activateCursor()
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) {
        if (t === "s" || t === "S") root.toggleActive()
      }

      Column {
        id: column
        anchors.fill: parent
        spacing: Style.space(14)

        // ---------- Hero: cast icon · status · search switch ----------
        Item {
          width: parent.width
          implicitHeight: Math.max(heroIcon.implicitHeight, heroLabels.implicitHeight, powerSwitch.implicitHeight)

          Text {
            id: heroIcon
            textFormat: Text.PlainText
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
            text: root.barIcon
            color: root.service.phase === "error" ? Commons.Color.urgent : root.foreground
            font.family: root.fontFamily
            font.pixelSize: Style.font.display
            opacity: root.active ? 1.0 : 0.5
          }

          ToggleSwitch {
            id: powerSwitch
            checked: root.active
            hasCursor: root.headerHasCursor
            foreground: root.foreground
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            onHovered: function(on) { if (on) root.setHeaderCursor() }
            onToggled: root.toggleActive()

            PanelToolTip {
              visible: powerSwitch.containsMouse
              text: root.active ? "Desactivar envío de pantalla" : "Activar envío de pantalla"
              fontFamily: root.fontFamily
            }
          }

          Column {
            id: heroLabels
            anchors.left: heroIcon.right
            anchors.leftMargin: Style.space(14)
            anchors.right: parent.right
            anchors.rightMargin: powerSwitch.width + Style.space(12)
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(2)

            Text {
              text: "Enviar pantalla"
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.title
              font.bold: true
              elide: Text.ElideRight
              width: parent.width
            }

            Text {
              id: heroStatus
              textFormat: Text.PlainText
              text: root.heroStatusText.toUpperCase()
              color: Qt.darker(root.foreground, 1.4)
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              font.bold: true
              font.letterSpacing: 1.2
              elide: Text.ElideRight
              width: parent.width
            }
          }
        }

        Text {
          width: parent.width
          visible: root.service.warning === ""
            && (root.service.phase === "error" || (root.service.busy && !root.service.streaming))
          text: root.service.detail
          textFormat: Text.PlainText
          wrapMode: Text.Wrap
          maximumLineCount: 2
          elide: Text.ElideRight
          color: Qt.darker(root.foreground, 1.4)
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        Text {
          width: parent.width
          visible: root.service.warning !== ""
          text: root.service.warning
          textFormat: Text.PlainText
          wrapMode: Text.Wrap
          color: Commons.Color.urgent
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        PanelSeparator { foreground: root.foreground }

        // ---------- Connected session ----------
        Column {
          id: connectedSection
          width: parent.width
          spacing: Style.space(10)
          visible: root.connectedReceiver !== null

          PanelSectionHeader {
            text: "CONECTADO"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          DeviceRow {
            width: parent.width
            dev: root.connectedReceiver
            rowIndex: 0
            sectionName: "connected"
          }

          Text {
            width: parent.width
            visible: root.service.streaming
            text: root.service.resolution === "AirPlay"
              ? (root.service.airplayAudio
                  ? "AirPlay · audio y vídeo. El audio también suena en la laptop; silencia la laptop para oír solo la TV."
                  : "AirPlay · vídeo sin audio.")
              : root.service.resolution + " · " + Model.bitrate(root.service.kbps)
                + (root.service.feedback ? " · RTCP" : "")
                + ". El audio también suena en la laptop."
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            color: Qt.darker(root.foreground, 1.4)
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
          }
        }

        // ---------- PIN / password ----------
        Column {
          width: parent.width
          spacing: Style.space(6)
          visible: root.service.pinRequired
          onVisibleChanged: if (visible) Qt.callLater(pinInput.forceActiveFocus)

          PanelSectionHeader {
            text: root.service.pinKind === "password" ? "CONTRASEÑA DE LA TV" : "PIN DE LA TV"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          Ui.TextField {
            id: pinInput
            width: parent.width
            placeholderText: root.service.pinKind === "password" ? "Contraseña" : "PIN"
            inputMethodHints: root.service.pinKind === "password" ? Qt.ImhNone : Qt.ImhDigitsOnly
            maximumLength: 128
            foreground: root.foreground
            onAccepted: {
              root.service.submitPin(text)
              text = ""
            }
          }

          Ui.Button {
            width: parent.width
            text: "Enviar código"
            bordered: true
            foreground: root.foreground
            onClicked: {
              root.service.submitPin(pinInput.text)
              pinInput.text = ""
            }
          }
        }

        PanelSeparator {
          visible: root.connectedReceiver !== null && root.availableReceivers.length > 0
          foreground: root.foreground
        }

        // ---------- Available receivers ----------
        Column {
          id: availableSection
          width: parent.width
          spacing: Style.space(10)
          visible: root.availableReceivers.length > 0

          PanelSectionHeader {
            text: "DISPONIBLES"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          ListView {
            id: availableList
            width: parent.width
            height: Math.min(contentHeight, Style.space(400))
            spacing: Style.space(10)
            clip: true
            boundsBehavior: Flickable.StopAtBounds
            interactive: contentHeight > height
            ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

            model: root.availableReceivers
            currentIndex: root.scrollRowIndex
            onCurrentIndexChanged: if (currentIndex >= 0) Qt.callLater(keepCurrentVisible)
            function keepCurrentVisible() {
              if (currentIndex >= 0) positionViewAtIndex(currentIndex, ListView.Contain)
            }

            delegate: DeviceRow {
              required property var modelData
              required property int index
              width: availableList.width
              dev: modelData
              rowIndex: index
              sectionName: "available"
            }
          }
        }

        Text {
          width: parent.width
          visible: root.connectedReceiver === null && root.availableReceivers.length === 0
          text: !root.service.enabled ? "Activa el envío de pantalla para buscar TVs."
            : root.service.discovering
            ? "Buscando TVs AirPlay y Miracast…"
            : "Pon la TV en AirPlay o Compartir pantalla. Selecciona una TV para conectar."
          textFormat: Text.PlainText
          wrapMode: Text.Wrap
          color: Qt.darker(root.foreground, 1.5)
          font.family: root.fontFamily
          font.pixelSize: Style.font.bodySmall
        }

        // ---------- Diagnostics (secondary) ----------
        Text {
          width: parent.width
          text: root.showDiagnostics ? "Ocultar depuración" : "Depuración"
          textFormat: Text.PlainText
          color: Qt.darker(root.foreground, 1.5)
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption

          MouseArea {
            anchors.fill: parent
            cursorShape: Qt.PointingHandCursor
            onClicked: root.toggleDiagnostics()
          }
        }

        Flickable {
          width: parent.width
          height: root.showDiagnostics ? Math.min(diagnosticsColumn.implicitHeight, Style.space(260)) : 0
          visible: root.showDiagnostics
          contentHeight: diagnosticsColumn.implicitHeight
          clip: true
          interactive: contentHeight > height

          Column {
            id: diagnosticsColumn
            width: parent.width
            spacing: Style.space(6)

            Ui.Button {
              width: parent.width
              text: root.service.testPattern ? "✓ Patrón de prueba (sin capturar pantalla)" : "Patrón de prueba (sin capturar pantalla)"
              selected: root.service.testPattern
              bordered: true
              foreground: root.foreground
              enabled: !root.service.busy
              onClicked: root.service.testPattern = !root.service.testPattern
            }

            Text {
              width: parent.width
              visible: root.service.logPath !== ""
              text: "Registro: " + root.service.logPath
              textFormat: Text.PlainText
              wrapMode: Text.WrapAnywhere
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
            }

            Repeater {
              model: root.service.checks
              delegate: Text {
                required property var modelData
                width: diagnosticsColumn.width
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
              text: root.service.history.join("\n")
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

  // Two-line receiver row with the same chrome as the Bluetooth panel.
  component DeviceRow: CursorSurface {
    id: row
    required property var dev
    required property int rowIndex
    required property string sectionName

    readonly property bool isConnected: sectionName === "connected"
    readonly property bool rowSelected: root.cursorActive
      && root.focusSection === sectionName
      && root.selectedIndex === rowIndex
    readonly property bool canActivate: !!dev && (isConnected || (!root.service.busy
      && (dev.protocol === "airplay" ? root.service.airplayReady : root.service.prerequisitesReady)))
    readonly property string actionTooltip: !dev ? "" : (isConnected ? "Desconectar" : (canActivate ? "Conectar" : ""))

    readonly property string statusText: {
      if (!dev) return ""
      if (isConnected) {
        if (root.service.phase === "pairing") return "Emparejando…"
        if (root.service.phase === "stopping") return "Desconectando…"
        if (root.service.streaming) return "Enviando pantalla"
        return "Conectando…"
      }
      if (root.service.busy) return ""
      return Model.protocolLabel(dev.protocol)
        + (dev.protocol === "airplay"
          ? (dev.model ? " · " + dev.model : "")
          : " · " + dev.strength + "%")
    }

    readonly property color statusColor: isConnected
      ? root.foreground
      : Qt.darker(root.foreground, 1.5)

    hasCursor: rowSelected && canActivate
    current: isConnected
    foreground: root.foreground
    fill: root.hoverFill
    currentFill: root.selectedFill
    opacity: canActivate || isConnected ? 1.0 : 0.6

    implicitHeight: rowContent.implicitHeight + Style.spacing.rowPaddingX
    Accessible.role: Accessible.Button
    Accessible.name: (dev ? dev.name : "TV") + (isConnected ? " · Desconectar" : " · Conectar")

    MouseArea {
      id: rowMouse
      anchors.fill: parent
      hoverEnabled: true
      enabled: row.canActivate
      cursorShape: row.canActivate ? Qt.PointingHandCursor : Qt.ArrowCursor

      onContainsMouseChanged: if (containsMouse && row.canActivate) {
        root.cursorActive = true
        root.focusSection = row.sectionName
        root.selectedIndex = row.rowIndex
      }

      onClicked: root.activateReceiver(row.dev ? row.dev.key : "")
    }

    PanelToolTip {
      visible: rowMouse.containsMouse && row.actionTooltip !== ""
      text: row.actionTooltip
      fontFamily: root.fontFamily
    }

    Item {
      id: rowContent
      anchors.left: parent.left
      anchors.right: parent.right
      anchors.verticalCenter: parent.verticalCenter
      anchors.leftMargin: Style.space(10)
      anchors.rightMargin: Style.space(10)
      implicitHeight: Math.max(deviceIcon.implicitHeight, info.implicitHeight)

      Text {
        id: deviceIcon
        textFormat: Text.PlainText
        text: row.isConnected ? "󰄙" : "󰄘"
        color: row.statusColor
        font.family: root.fontFamily
        font.pixelSize: Style.font.heading
        anchors.left: parent.left
        anchors.verticalCenter: parent.verticalCenter
      }

      Column {
        id: info
        spacing: Style.space(1)
        anchors.left: deviceIcon.right
        anchors.leftMargin: Style.space(10)
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter

        Text {
          textFormat: Text.PlainText
          text: row.dev && row.dev.name ? row.dev.name : "TV"
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: Style.font.body
          elide: Text.ElideRight
          width: parent.width
        }

        Text {
          textFormat: Text.PlainText
          visible: row.statusText !== ""
          text: row.statusText
          color: row.statusColor
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
          elide: Text.ElideRight
          width: parent.width
        }
      }
    }
  }
}
