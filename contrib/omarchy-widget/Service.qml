import QtQuick
import Quickshell
import Quickshell.Io
import "Config.js" as Config

// The widget's data: one `fizz bridge` per shell, however many monitors show
// the bar. Fizz prints its state as a line of JSON whenever it changes and
// takes requests as lines of JSON, so nothing here talks to the server, holds
// the password, or puts message text in a command line. The contract is at
// the top of src/bridge.rs.
Item {
  id: root

  property var shell: null
  property var settings: ({})
  // A widget-local copy, built only under a shell without shared services,
  // stays inert while a shared one exists.
  property bool active: true

  // Nothing starts until a widget has said which mode it wants, and then
  // only after a moment: a widget put in the bar and switched to the demo
  // straight afterwards should never start the real thing in between.
  property bool configured: false
  property bool demo: false
  readonly property bool wanted: active && configured

  readonly property var empty: ({ v: 1, demo: false, online: null, loading: true, error: null, unread: 0, chats: [], open: null })
  property var state: empty
  readonly property bool running: bridge.running
  // Why there is nothing to show, when the bridge is not running.
  property string problem: ""
  property int failures: 0
  property bool restarting: false

  function configure(s) {
    var wantDemo = !!(s && s.demo === true)
    if (configured && wantDemo === demo) return
    demo = wantDemo
    configured = true
    if (bridge.running) {
      restarting = true
      bridge.running = false
    } else {
      settle.restart()
    }
  }

  function launch() {
    if (!wanted || bridge.running) return
    problem = ""
    bridge.command = [Config.fizz, "bridge"].concat(demo ? ["--demo"] : [])
    bridge.running = true
  }

  function take(line) {
    var next
    try { next = JSON.parse(line) } catch (e) { return }
    if (!next || typeof next !== "object") return
    if (next.v !== 1) {
      problem = "Fizz and this widget are different versions. Run `fizz widget install`, then `omarchy restart shell`."
      return
    }
    failures = 0
    state = next
  }

  function request(what) {
    if (bridge.running) bridge.write(JSON.stringify(what) + "\n")
  }
  function openChat(id) { request({ cmd: "open", chat: id }) }
  function closeChat() { request({ cmd: "close" }) }
  function sendText(id, text) { request({ cmd: "send", chat: id, text: text }) }
  function retry(id, message) { request({ cmd: "retry", chat: id, message: message }) }
  function refresh() { request({ cmd: "refresh" }) }

  onWantedChanged: {
    if (wanted) settle.restart()
    else if (bridge.running) bridge.running = false
  }

  Timer {
    id: settle
    interval: 1000
    onTriggered: root.launch()
  }

  Process {
    id: bridge
    stdinEnabled: true
    stdout: SplitParser { onRead: function(line) { root.take(line) } }
    // The first thing it says is the reason, if it then stops. An older
    // Fizz, for one, does not know the word "bridge".
    stderr: SplitParser {
      onRead: function(line) {
        if (root.problem === "" && line.trim() !== "") root.problem = line.trim()
      }
    }
    onExited: function(exitCode, exitStatus) {
      root.state = root.empty
      if (root.restarting) {
        root.restarting = false
        root.launch()
      } else if (root.wanted) {
        if (root.problem === "") root.problem = "Fizz stopped"
        root.failures += 1
        again.restart()
      }
    }
  }

  // Try again, less and less often: 4s, 8s, … up to about a minute.
  Timer {
    id: again
    interval: Math.min(64000, 2000 * Math.pow(2, Math.min(root.failures, 5)))
    onTriggered: root.launch()
  }
}
