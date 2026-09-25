import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui
import "../Model.js" as Model

Panel {
  id: root
  moduleName: "raja.voisu-local"
  ipcTarget: "raja.voisu-local"
  manageIpc: false

  property var anchorItem: null
  property var hostWidget: null
  readonly property var barIdentity: hostWidget || root

  property var status: Model.emptyStatus()
  property string lastError: ""
  property string pendingSelectId: ""
  property bool selecting: false
  property int cursorIndex: 0

  readonly property string voisuPath: Model.voisuBinary(setting("voisuBinary", Model.defaultVoisuBinary()))
  readonly property var models: status && status.models ? status.models : []
  readonly property var selectedId: status && status.selectedId ? status.selectedId : null
  readonly property string barLabel: Model.barLabel(status)
  readonly property color contentForeground: bar ? bar.foreground : Color.foreground
  readonly property color urgent: bar ? bar.urgent : Color.urgent
  readonly property color dim: Qt.darker(contentForeground, 1.55)
  readonly property string contentFontFamily: bar ? bar.fontFamily : Style.font.family
  readonly property bool busy: statusProcess.running || selectProcess.running || selecting

  function open() {
    root.refresh()
    root.controller.show()
    Qt.callLater(function() {
      if (root.opened) setCenterHoverRevealSuppressed(true)
    })
  }

  function close() {
    setCenterHoverRevealSuppressed(false)
    root.controller.hide()
  }

  function toggle() {
    if (root.opened) root.close()
    else root.open()
  }

  function switchPanel(direction) {
    if (root.bar && typeof root.bar.switchPanelFrom === "function")
      return root.bar.switchPanelFrom(root.barIdentity, direction)
    return false
  }

  function setCenterHoverRevealSuppressed(value) {
    if (root.bar && typeof root.bar.setCenterHoverRevealSuppressed === "function")
      root.bar.setCenterHoverRevealSuppressed(value)
    else if (root.bar && "centerHoverRevealSuppressed" in root.bar)
      root.bar.centerHoverRevealSuppressed = value
  }

  function applyStatus(raw) {
    var parsed = Model.parseStatus(raw)
    root.status = parsed
    if (root.cursorIndex >= parsed.models.length)
      root.cursorIndex = Math.max(0, parsed.models.length - 1)
  }

  function refresh() {
    if (statusProcess.running) return
    statusProcess.command = Model.statusCommand(root.voisuPath)
    statusProcess.running = true
  }

  function moveCursor(dy) {
    if (!root.models.length) return
    var next = root.cursorIndex + dy
    if (next < 0) next = 0
    if (next >= root.models.length) next = root.models.length - 1
    root.cursorIndex = next
  }

  function selectModel(modelId) {
    var row = Model.findModel(root.status, modelId)
    var decision = Model.decideSelect(row)
    if (!decision.ok) {
      root.lastError = decision.error
      return
    }
    if (selectProcess.running || root.selecting) return
    root.lastError = ""
    root.pendingSelectId = decision.modelId
    root.selecting = true
    selectProcess.command = Model.selectCommand(root.voisuPath, decision.modelId)
    selectProcess.running = true
  }

  function activateCursor() {
    if (!root.models.length) return
    var row = root.models[Math.max(0, Math.min(root.cursorIndex, root.models.length - 1))]
    if (row) root.selectModel(row.model_id)
  }

  function finishSelect(exitCode, stdout, stderr) {
    var outcome = Model.applySelectOutcome(root.status, root.pendingSelectId, {
      code: exitCode,
      stdout: stdout,
      stderr: stderr
    })
    root.selecting = false
    root.pendingSelectId = ""
    if (outcome.ok) {
      root.lastError = ""
      root.refresh()
      return
    }
    root.lastError = outcome.error
  }

  Timer {
    id: refreshTimer
    interval: 4000
    repeat: true
    running: true
    triggeredOnStart: true
    onTriggered: root.refresh()
  }

  Process {
    id: statusProcess
    running: false
    command: []
    stdout: StdioCollector { id: statusStdout; waitForEnd: true }
    stderr: StdioCollector { id: statusStderr; waitForEnd: true }
    onExited: function(exitCode) {
      var stdout = String(statusStdout.text || "")
      var stderr = String(statusStderr.text || "").replace(/^\s+|\s+$/g, "")
      if (exitCode === 0) {
        root.applyStatus(stdout)
        if (root.lastError !== "" && !Model.isBusyError(root.lastError))
          root.lastError = ""
      } else if (root.lastError === "") {
        root.lastError = stderr || "Could not read voisu local-dev status"
      }
    }
  }

  Process {
    id: selectProcess
    running: false
    command: []
    stdout: StdioCollector { id: selectStdout; waitForEnd: true }
    stderr: StdioCollector { id: selectStderr; waitForEnd: true }
    onExited: function(exitCode) {
      root.finishSelect(exitCode, String(selectStdout.text || ""), String(selectStderr.text || ""))
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    centerOnBar: false
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(360))
    contentHeight: panel.fittedContentHeight(contentColumn.implicitHeight)

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onMoveRequested: function(dx, dy) {
        if (dy !== 0) root.moveCursor(dy)
      }
      onActivateRequested: root.activateCursor()
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) {
        if (t === "r" || t === "R") root.refresh()
      }

      Flickable {
        id: panelFlick
        anchors.fill: parent
        contentWidth: width
        contentHeight: contentColumn.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        flickableDirection: Flickable.VerticalFlick
        interactive: contentHeight > height

        Column {
          id: contentColumn
          width: panelFlick.width
          spacing: Style.space(12)

          PanelHero {
            width: parent.width
            title: root.barLabel
            meta: root.selectedId ? root.selectedId : "No model selected"
            detail: root.busy ? "Working…" : ""
            foreground: root.contentForeground
            fontFamily: root.contentFontFamily
            iconComponent: Component {
              Text {
                text: "󰍬"
                color: root.contentForeground
                font.family: root.contentFontFamily
                font.pixelSize: Style.font.display
              }
            }
          }

          Text {
            visible: root.lastError !== ""
            width: parent.width
            text: root.lastError
            color: root.urgent
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.WordWrap
          }

          PanelSeparator {
            foreground: root.contentForeground
          }

          PanelSectionHeader {
            text: "DEVELOPMENT MODELS"
            foreground: root.contentForeground
            fontFamily: root.contentFontFamily
          }

          Column {
            id: modelList
            width: parent.width
            spacing: Style.space(6)

            Repeater {
              model: root.models

              Rectangle {
                required property var modelData
                required property int index
                width: modelList.width
                implicitHeight: rowColumn.implicitHeight + Style.space(12)
                radius: Style.cornerRadius
                color: {
                  if (modelData.selected) return Style.selectedFillFor(root.contentForeground, Color.accent)
                  if (index === root.cursorIndex || rowMouse.containsMouse)
                    return Style.hoverFillFor(root.contentForeground, Color.accent)
                  return Style.normalFillFor(root.contentForeground, Color.accent)
                }
                opacity: modelData.registered ? 1 : 0.55
                border.width: modelData.selected ? Style.selectedBorderWidth : (index === root.cursorIndex ? Style.hoverBorderWidth : Style.normalBorderWidth)
                border.color: modelData.selected
                  ? Style.selectedBorderFor(root.contentForeground, Color.accent)
                  : (index === root.cursorIndex
                    ? Style.hoverBorderFor(root.contentForeground, Color.accent)
                    : Style.normalBorderFor(root.contentForeground, Color.accent))

                Column {
                  id: rowColumn
                  anchors.left: parent.left
                  anchors.right: parent.right
                  anchors.verticalCenter: parent.verticalCenter
                  anchors.leftMargin: Style.space(10)
                  anchors.rightMargin: Style.space(10)
                  spacing: Style.space(2)

                  Row {
                    width: parent.width
                    spacing: Style.space(8)

                    Text {
                      text: modelData.shortLabel
                      color: root.contentForeground
                      font.family: root.contentFontFamily
                      font.pixelSize: Style.font.body
                      font.bold: modelData.selected
                    }

                    Text {
                      visible: modelData.selected
                      text: "selected"
                      color: Color.accent
                      font.family: root.contentFontFamily
                      font.pixelSize: Style.font.caption
                      anchors.verticalCenter: parent.verticalCenter
                    }
                  }

                  Text {
                    width: parent.width
                    text: modelData.model_id
                    color: root.dim
                    font.family: root.contentFontFamily
                    font.pixelSize: Style.font.caption
                    wrapMode: Text.WrapAnywhere
                  }

                  Text {
                    visible: !modelData.registered && modelData.error && modelData.error !== ""
                    width: parent.width
                    text: modelData.error || ""
                    color: root.urgent
                    font.family: root.contentFontFamily
                    font.pixelSize: Style.font.caption
                    wrapMode: Text.WordWrap
                  }
                }

                MouseArea {
                  id: rowMouse
                  anchors.fill: parent
                  hoverEnabled: true
                  cursorShape: modelData.registered ? Qt.PointingHandCursor : Qt.ArrowCursor
                  onEntered: root.cursorIndex = index
                  onClicked: {
                    root.cursorIndex = index
                    root.selectModel(modelData.model_id)
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}
