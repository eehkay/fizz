import QtQuick
import QtQuick.Layouts
import Quickshell
import qs.Commons
import qs.Commons as Commons
import qs.Ui
import "Config.js" as Config

// Fizz in the bar: an unread count, and a panel with the recent
// conversations and a reply box. Everything shown comes from Service.qml,
// which runs `fizz bridge`; this file only draws it and passes on requests.
//
// Replies only, on purpose. Starting a conversation means choosing who it
// goes to, which belongs in the full app, so the panel has a button for that.
Panel {
  id: root
  moduleName: "eehkay.fizz"
  ipcTarget: "eehkay.fizz"

  // One service per shell. A shell without shared services gets a local one.
  readonly property var sharedService: bar && bar.shell && typeof bar.shell.serviceFor === "function"
    ? bar.shell.serviceFor(moduleName) : null
  readonly property var service: sharedService || localService
  Service {
    id: localService
    active: root.sharedService === null
  }
  function configure() { if (service) service.configure(settings) }
  onServiceChanged: configure()
  onSettingsChanged: configure()
  Component.onCompleted: configure()

  readonly property var st: service ? service.state : null
  readonly property var chats: st && st.chats ? st.chats : []
  readonly property int unread: st ? (Number(st.unread) || 0) : 0
  readonly property bool demo: setting("demo", false) === true
  readonly property bool showCount: setting("showCount", true) !== false
  readonly property bool stopped: !service || !service.running
  readonly property bool offline: !stopped && st && st.online === false
  readonly property string trouble: stopped ? ((service && service.problem) || "Fizz is not running")
    : (st && st.error && chats.length === 0 ? "Can't reach the server: " + st.error : "")

  // "list", or "chat" with the conversation being shown.
  property string view: "list"
  property string chatId: ""
  property int cursor: -1
  readonly property var chat: {
    for (var i = 0; i < chats.length; i++) if (chats[i].id === chatId) return chats[i]
    return null
  }
  // The service is shared between monitors, so only believe the open
  // conversation when it is the one this panel asked for.
  readonly property var convo: view === "chat" && st && st.open && st.open.id === chatId ? st.open : null
  readonly property string chatTitle: convo ? convo.title : (chat ? chat.title : "")
  readonly property var messages: convo && convo.messages ? convo.messages : []

  readonly property color ink: Commons.Color.popups.text
  readonly property color accent: Commons.Color.accent
  readonly property color urgentColor: bar ? bar.urgent : Commons.Color.urgent

  function showChat(id) {
    if (!service || !id) return
    chatId = id
    view = "chat"
    service.openChat(id)
    Qt.callLater(function() {
      var c = panel.loadedContent
      if (c && c.reply) { c.reply.text = ""; c.reply.forceActiveFocus() }
    })
  }

  function showList() {
    if (service && view === "chat") service.closeChat()
    view = "list"
    chatId = ""
    Qt.callLater(function() { if (panel.loadedContent) panel.loadedContent.forceActiveFocus() })
  }

  function moveCursor(dy) {
    if (view !== "list" || chats.length === 0) return
    cursor = Math.max(0, Math.min(chats.length - 1, cursor + dy))
  }

  function activate() {
    if (view === "list" && cursor >= 0 && cursor < chats.length) showChat(chats[cursor].id)
  }

  function sendReply() {
    var c = panel.loadedContent
    if (!c || !c.reply || !service || chatId === "") return
    var text = c.reply.text.trim()
    if (text === "") return
    service.sendText(chatId, text)
    c.reply.text = ""
  }

  function launchFizz() {
    if (!bar) return
    close()
    bar.run("omarchy-launch-or-focus-tui --app-id=org.omarchy.fizz " + Util.shellQuote(Config.fizz) + (demo ? " --demo" : ""))
  }

  function sameDay(a, b) {
    return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate()
  }

  function clock(d) { return d.toLocaleTimeString(Qt.locale(), Locale.ShortFormat) }

  // When a conversation last moved, as briefly as a list row can say it.
  function when(ms) {
    if (!ms) return ""
    var d = new Date(Number(ms))
    var now = new Date()
    if (sameDay(d, now)) return clock(d)
    var yesterday = new Date(now.getFullYear(), now.getMonth(), now.getDate() - 1)
    if (sameDay(d, yesterday)) return "Yesterday"
    if (now.getTime() - d.getTime() < 6 * 86400000) return Qt.formatDate(d, "dddd")
    return Qt.formatDate(d, d.getFullYear() === now.getFullYear() ? "MMM d" : "MMM d, yyyy")
  }

  // A fuller one, for between messages.
  function stamp(ms) {
    var d = new Date(Number(ms))
    var day = when(ms)
    return sameDay(d, new Date()) ? day : day + " · " + clock(d)
  }

  function bubbleText(m) {
    var parts = []
    var files = m.attachments || []
    for (var i = 0; i < files.length; i++) parts.push("[" + files[i] + "]")
    if (m.text) parts.push(m.text)
    return parts.join("\n")
  }

  function tooltip() {
    if (trouble !== "") return "Fizz: " + trouble
    if (offline) return "Fizz: offline"
    if (unread === 0) return "Fizz: nothing unread"
    return "Fizz: " + unread + (unread === 1 ? " unread conversation" : " unread conversations")
  }

  onOpenedChanged: {
    if (opened) {
      cursor = -1
      view = "list"
      chatId = ""
      Qt.callLater(function() { if (panel.loadedContent) panel.loadedContent.forceActiveFocus() })
    } else {
      // Nothing is on screen any more, so nothing new should count as read.
      if (service && view === "chat") service.closeChat()
      view = "list"
      chatId = ""
    }
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "󰍡"
    labelText: root.showCount && root.unread > 0 && !vertical ? String(root.unread) : ""
    slotSize: Style.bar.iconSlot + (labelText !== "" ? Style.space(6 + 7 * labelText.length) : 0)
    active: root.unread > 0
    activeColor: root.accent
    dimmed: root.stopped || root.offline
    tooltipText: root.tooltip()
    onPressed: function(b) {
      if (b === Qt.RightButton) root.launchFizz()
      else root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: panel.loadedContent
    contentWidth: panel.fittedContentWidth(Style.space(360))
    contentHeight: panel.fittedContentHeight(panel.loadedContentHeight)

    content: PanelKeyCatcher {
      id: keys
      property alias reply: reply
      anchors.fill: parent
      implicitHeight: column.implicitHeight
      // While a reply is being typed, every key is the field's.
      blocked: reply.activeFocus
      onMoveRequested: function(dx, dy) { if (dy !== 0) root.moveCursor(dy) }
      onActivateRequested: root.activate()
      onCloseRequested: root.view === "chat" ? root.showList() : root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) {
        if (root.view === "chat" && (t === "r" || t === "i")) reply.forceActiveFocus()
        else if (t === "o") root.launchFizz()
      }

      ColumnLayout {
        id: column
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        spacing: Style.spacing.md

        // ---------- Header ----------
        RowLayout {
          Layout.fillWidth: true
          spacing: Style.spacing.sm

          Button {
            visible: root.view === "chat"
            iconText: "󰅁"
            tooltipText: "Back to conversations (Esc)"
            onClicked: root.showList()
          }
          Text {
            visible: root.view === "list"
            textFormat: Text.PlainText
            text: "󰍡"
            color: root.unread > 0 ? root.accent : root.ink
            font.family: Style.font.family
            font.pixelSize: Style.font.icon
          }
          Text {
            Layout.fillWidth: true
            textFormat: Text.PlainText
            text: root.view === "chat" ? root.chatTitle : "Messages"
            color: root.ink
            font.family: Style.font.family
            font.pixelSize: Style.font.title
            font.bold: true
            elide: Text.ElideRight
          }
          Text {
            textFormat: Text.PlainText
            visible: text !== ""
            text: root.demo ? "Demo" : (root.offline ? "Offline" : (root.view === "list" && root.unread > 0 ? root.unread + " unread" : ""))
            color: root.offline ? root.urgentColor : (root.demo ? root.accent : root.ink)
            opacity: root.offline || root.demo ? 1 : 0.6
            font.family: Style.font.family
            font.pixelSize: Style.font.bodySmall
          }
        }

        // ---------- Something is wrong ----------
        Text {
          Layout.fillWidth: true
          visible: root.trouble !== ""
          textFormat: Text.PlainText
          text: root.trouble
          color: root.urgentColor
          font.family: Style.font.family
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.Wrap
          maximumLineCount: 4
          elide: Text.ElideRight
        }

        // ---------- The conversations ----------
        Text {
          Layout.fillWidth: true
          visible: root.view === "list" && root.chats.length === 0 && root.trouble === ""
          textFormat: Text.PlainText
          text: root.st && root.st.loading ? "Loading…" : "No conversations yet"
          color: root.ink
          opacity: 0.6
          font.family: Style.font.family
          font.pixelSize: Style.font.bodySmall
        }

        ColumnLayout {
          Layout.fillWidth: true
          visible: root.view === "list" && root.chats.length > 0
          spacing: Style.spacing.xxs

          Repeater {
            model: root.view === "list" ? root.chats : []
            delegate: Item {
              id: chatRow
              required property var modelData
              required property int index
              readonly property bool current: root.cursor === index
              Layout.fillWidth: true
              implicitHeight: rowBody.implicitHeight + Style.space(10)

              Rectangle {
                anchors.fill: parent
                radius: Style.cornerRadius
                color: root.ink
                opacity: chatRow.current ? 0.14 : (rowMouse.containsMouse ? 0.08 : 0)
              }
              RowLayout {
                id: rowBody
                anchors.verticalCenter: parent.verticalCenter
                anchors.left: parent.left
                anchors.right: parent.right
                anchors.leftMargin: Style.space(6)
                anchors.rightMargin: Style.space(6)
                spacing: Style.spacing.md

                Rectangle {
                  Layout.alignment: Qt.AlignVCenter
                  width: Style.space(7)
                  height: width
                  radius: width / 2
                  color: chatRow.modelData.unread ? root.accent : "transparent"
                }
                ColumnLayout {
                  Layout.fillWidth: true
                  spacing: Style.spacing.xxs
                  RowLayout {
                    Layout.fillWidth: true
                    spacing: Style.spacing.sm
                    Text {
                      Layout.fillWidth: true
                      textFormat: Text.PlainText
                      text: chatRow.modelData.title || ""
                      color: root.ink
                      font.family: Style.font.family
                      font.pixelSize: Style.font.body
                      font.bold: chatRow.modelData.unread === true
                      elide: Text.ElideRight
                    }
                    Text {
                      textFormat: Text.PlainText
                      text: root.when(chatRow.modelData.time)
                      color: chatRow.modelData.unread ? root.accent : root.ink
                      opacity: chatRow.modelData.unread ? 1 : 0.5
                      font.family: Style.font.family
                      font.pixelSize: Style.font.caption
                    }
                  }
                  Text {
                    Layout.fillWidth: true
                    textFormat: Text.PlainText
                    text: (chatRow.modelData.from_me ? "You: " : "") + (chatRow.modelData.preview || "")
                    color: root.ink
                    opacity: chatRow.modelData.unread ? 0.9 : 0.6
                    font.family: Style.font.family
                    font.pixelSize: Style.font.bodySmall
                    elide: Text.ElideRight
                    maximumLineCount: 1
                  }
                }
              }
              MouseArea {
                id: rowMouse
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onClicked: root.showChat(chatRow.modelData.id)
              }
            }
          }
        }

        // ---------- One conversation ----------
        Text {
          Layout.fillWidth: true
          visible: root.view === "chat" && root.messages.length === 0
          textFormat: Text.PlainText
          text: root.convo && root.convo.error ? "Couldn't load this conversation: " + root.convo.error
            : (!root.convo || root.convo.loading ? "Loading…" : "No messages yet")
          color: root.convo && root.convo.error ? root.urgentColor : root.ink
          opacity: root.convo && root.convo.error ? 1 : 0.6
          font.family: Style.font.family
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.Wrap
        }

        Flickable {
          id: scroller
          Layout.fillWidth: true
          Layout.preferredHeight: Math.min(thread.implicitHeight, Style.space(340))
          visible: root.view === "chat" && root.messages.length > 0
          contentWidth: width
          contentHeight: thread.implicitHeight
          clip: true
          boundsBehavior: Flickable.StopAtBounds
          // Stay on the newest message as the conversation grows.
          function toEnd() { contentY = Math.max(0, contentHeight - height) }
          onContentHeightChanged: toEnd()
          onHeightChanged: toEnd()

          Column {
            id: thread
            width: scroller.width
            spacing: Style.spacing.sm

            Repeater {
              model: root.messages
              delegate: Column {
                id: entry
                required property var modelData
                required property int index
                readonly property var before: index > 0 ? root.messages[index - 1] : null
                readonly property bool mine: modelData.from_me === true
                readonly property bool failed: !!modelData.failed
                // A time between messages more than half an hour apart.
                readonly property bool gap: !before || Number(modelData.time) - Number(before.time) > 30 * 60000
                readonly property bool named: root.convo && root.convo.group && !mine && !modelData.note
                  && modelData.sender !== "" && (gap || !before || before.from_me || before.note || before.sender !== modelData.sender)
                width: thread.width
                spacing: Style.spacing.xxs

                Text {
                  visible: entry.gap
                  width: parent.width
                  horizontalAlignment: Text.AlignHCenter
                  textFormat: Text.PlainText
                  text: root.stamp(entry.modelData.time)
                  color: root.ink
                  opacity: 0.45
                  font.family: Style.font.family
                  font.pixelSize: Style.font.caption
                }
                Text {
                  visible: entry.modelData.note === true
                  width: parent.width
                  horizontalAlignment: Text.AlignHCenter
                  textFormat: Text.PlainText
                  text: entry.modelData.text || ""
                  color: root.ink
                  opacity: 0.55
                  font.family: Style.font.family
                  font.pixelSize: Style.font.caption
                  font.italic: true
                  wrapMode: Text.Wrap
                }
                Text {
                  visible: entry.named
                  textFormat: Text.PlainText
                  text: entry.modelData.sender || ""
                  color: root.accent
                  font.family: Style.font.family
                  font.pixelSize: Style.font.caption
                  leftPadding: Style.space(4)
                }
                Rectangle {
                  id: bubble
                  visible: entry.modelData.note !== true
                  anchors.right: entry.mine ? parent.right : undefined
                  anchors.left: entry.mine ? undefined : parent.left
                  width: Math.min(said.implicitWidth, Math.round(thread.width * 0.82))
                  height: said.height
                  radius: Style.cornerRadius > 0 ? Style.cornerRadius : Style.space(3)
                  color: entry.failed ? Util.alpha(root.urgentColor, 0.18)
                    : (entry.mine ? Util.alpha(root.accent, 0.24) : Util.alpha(root.ink, 0.09))
                  border.width: entry.failed ? 1 : 0
                  border.color: root.urgentColor
                  opacity: entry.modelData.pending ? 0.55 : 1

                  Text {
                    id: said
                    width: Math.min(implicitWidth, Math.round(thread.width * 0.82))
                    textFormat: Text.PlainText
                    text: root.bubbleText(entry.modelData)
                    color: root.ink
                    font.family: Style.font.family
                    font.pixelSize: Style.font.body
                    wrapMode: Text.Wrap
                    padding: Style.space(6)
                    leftPadding: Style.space(9)
                    rightPadding: Style.space(9)
                  }
                  MouseArea {
                    anchors.fill: parent
                    enabled: entry.failed
                    cursorShape: entry.failed ? Qt.PointingHandCursor : Qt.ArrowCursor
                    onClicked: if (root.service) root.service.retry(root.chatId, entry.modelData.id)
                  }
                }
                Text {
                  visible: text !== ""
                  anchors.right: entry.mine ? parent.right : undefined
                  anchors.left: entry.mine ? undefined : parent.left
                  textFormat: Text.PlainText
                  text: entry.failed ? "Not sent · click the message to try again"
                    : (entry.modelData.pending ? "Sending…" : (entry.modelData.reactions || []).join(" "))
                  color: entry.failed ? root.urgentColor : root.ink
                  opacity: entry.failed ? 1 : 0.7
                  font.family: Style.font.family
                  font.pixelSize: Style.font.caption
                  leftPadding: Style.space(4)
                  rightPadding: Style.space(4)
                }
              }
            }
          }
        }

        TextField {
          id: reply
          Layout.fillWidth: true
          visible: root.view === "chat"
          enabled: !root.stopped
          foreground: root.ink
          placeholderText: root.chatTitle !== "" ? "Reply to " + root.chatTitle : "Reply"
          onAccepted: root.sendReply()
          Keys.onEscapePressed: root.showList()
        }

        // ---------- Footer ----------
        PanelSeparator { Layout.fillWidth: true }

        RowLayout {
          Layout.fillWidth: true
          spacing: Style.spacing.sm
          Text {
            Layout.fillWidth: true
            textFormat: Text.PlainText
            text: root.view === "chat" ? "Enter sends · Esc goes back" : "New conversations start in Fizz"
            color: root.ink
            opacity: 0.5
            font.family: Style.font.family
            font.pixelSize: Style.font.caption
            elide: Text.ElideRight
          }
          Button {
            text: "Open Fizz"
            bordered: true
            onClicked: root.launchFizz()
          }
        }
      }
    }
  }
}
