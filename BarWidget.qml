import QtQuick
import Quickshell.Io
import qs.Ui

// The bar half of the Omarchy Session Memory plugin.
//
// It owns exactly one thing: the periodic `osm status --json` probe, and the
// state that probe puts the plugin in. Menu.qml renders it; nothing else
// polls the engine on a timer.
//
// **The plugin and the engine ship separately.** `omarchy plugin add` copies
// QML and nothing else — it cannot install a binary, a systemd unit or a tmux
// hook. So the first thing this widget has to be able to say is "the engine
// is not here", and it must say it rather than render an empty session list
// that looks like a machine with nothing to restore. Those two states look
// identical and mean opposite things.
//
// Everything is invoked as an argv array. Session names, conversation ids and
// window titles all pass through this plugin and are attacker-influenced in
// the general case; `bar.run()` and `Util.execDetached()` both end up as
// `bash -lc <string>`, so neither is used here.
BarWidget {
  id: root
  moduleName: "io.github.n8group-oss.sessionmemory"

  // ---- The compatibility contract.
  //
  // `protocol_version` is the engine's promise about the shape of its JSON.
  // This file was written against 1. A different major is not a version to
  // squint at — it is a shape this code has never seen, so it refuses to
  // render a session list from it and says which version it needs.
  readonly property int supportedProtocol: 1

  readonly property string osmPath: String(root.setting("osmPath", "osm"))
  readonly property int pollIntervalSec: Math.max(2, Math.min(300, parseInt(String(root.setting("pollIntervalSec", 5)), 10) || 5))

  // How long any one osm invocation is allowed to take before it is stopped
  // and reported as a failure.
  //
  // The engine takes a lock and opens a database. A lock held by a capture on
  // an unresponsive filesystem, or a home directory that has stopped
  // answering, is a process that never exits — and a shell that waits on it
  // forever goes on rendering whatever it last knew, with a healthy icon and
  // no sign that the number is old. Every process here is bounded.
  readonly property int probeTimeoutMs: Math.max(1000, Math.min(120000, parseInt(String(root.setting("probeTimeoutMs", 10000)), 10) || 10000))

  // ---- What the probe found.
  //
  // "unknown" until the first probe returns. It is deliberately not folded
  // into "missing": before the first answer the plugin does not know whether
  // the engine is there, and the whole point of this widget is that it never
  // renders an unknown as a no.
  //
  //   unknown       no probe has completed yet
  //   missing       /usr/bin/env could not find or execute the binary
  //   unreadable    something ran, but it did not produce status JSON
  //   incompatible  status JSON whose protocol_version is not supportedProtocol
  //   notReady      the engine answered and reports ready:false
  //   ready         the engine answered and reports ready:true
  property string engineState: "unknown"

  // The parsed status object, or null when there is nothing valid to show.
  //
  // Set only by `applyProbe`, and only once a response has passed every check
  // in `protocolFault` and `shapeFault`. Never a hand-built stand-in and never
  // a partial one: a widget that invents a status object, or publishes half of
  // one, renders reassurance it has no evidence for.
  property var status: null

  // The protocol the engine last announced, as text.
  //
  // Kept apart from `status` on purpose. An incompatible engine has to be able
  // to say *which* version it speaks, and that is the only field of an
  // unrecognised response this plugin is entitled to read — the rest of it is
  // a shape this code has never seen.
  property string protocolSeen: ""

  // Set by the watchdog when it stops a probe. The process may still emit an
  // exit afterwards; that exit describes the kill, not the engine, and is
  // ignored.
  property bool statusTimedOut: false

  // Verbatim text from the engine or from the failed invocation. Shown as-is
  // in the menu — paraphrasing an error is how a diagnosable failure becomes
  // an undiagnosable one.
  property string detail: ""

  // Consecutive probes that did not produce status JSON, and the epoch
  // milliseconds of the last completed probe (0 = never).
  property int failures: 0
  property double lastProbeAt: 0

  readonly property bool healthy: engineState === "ready"
  readonly property var sessions: (healthy && status && status.sessions) ? status.sessions : []
  readonly property int sessionCount: sessions.length

  // ---- Invocation.
  //
  // Every osm call goes through /usr/bin/env, for one reason beyond PATH
  // lookup: env's exit status distinguishes "there is no such program" (127)
  // and "it is not executable" (126) from anything the program itself
  // returns. Without that, a missing binary and a crashed one are the same
  // event, and the widget would have to guess which — exactly the kind of
  // guess this plugin exists not to make.
  //
  // `concat` on an array literal, never string interpolation: argv survives
  // intact, so a session name containing a space, a quote or a `$(…)` is one
  // argument and not a command.
  function argv(args) {
    return ["/usr/bin/env", root.osmPath].concat(args)
  }

  // ---- Backoff.
  //
  // A machine without the engine installed is the common case for a plugin
  // installed from the marketplace, and it is a state that does not change on
  // its own. Polling a binary that is not there every 5 seconds forever is a
  // process spawn per tick for no information. Each consecutive failure
  // doubles the interval up to five minutes; any successful parse resets it.
  readonly property int probeIntervalMs: {
    if (failures <= 0) return pollIntervalSec * 1000
    var backoff = pollIntervalSec * 1000 * Math.pow(2, Math.min(failures, 6))
    return Math.min(300000, backoff)
  }

  // Nerd Font glyphs, written literally as the shell's own widgets write
  // them: U+F1DA (clock with arrow) for a working engine, U+F127 (broken
  // link) for one that is not installed, U+F071 (warning) for one that is
  // there and cannot be used.
  readonly property string glyph: {
    if (engineState === "ready") return ""
    if (engineState === "missing" || engineState === "unknown") return ""
    return ""
  }

  // The count is rendered only when there is a count — an engine that has not
  // answered has no session count, and "0" would be a claim.
  readonly property string barLabel: {
    if (root.vertical) return glyph
    if (engineState !== "ready") return glyph
    return glyph + " " + sessionCount
  }

  readonly property string summary: {
    switch (engineState) {
    case "unknown":
      return "Session memory: checking for the osm engine…"
    case "missing":
      return "Session memory: the osm engine is not installed"
    case "unreadable":
      return "Session memory: the engine did not return status JSON"
    case "incompatible":
      return "Session memory: engine protocol " + protocolFound()
        + ", this plugin speaks " + supportedProtocol
    case "notReady":
      return "Session memory: engine not ready — "
        + (status && status.message ? String(status.message) : "no reason given")
    default:
      // A ready engine can still carry a message: `ready` means "the engine
      // can run", and notices that change what its answers mean — a preserved
      // database whose old snapshots are no longer readable — ride along with
      // it. The tooltip carries them rather than dropping them.
      var line = "Session memory: " + sessionCount
        + (sessionCount === 1 ? " session" : " sessions") + " recorded"
      if (status && status.message) line += " \u2014 " + String(status.message)
      return line
    }
  }

  function protocolFound() {
    return protocolSeen === "" ? "unknown" : protocolSeen
  }

  // ---- The response contract, at protocol 1.
  //
  // Every field `osm status --json` promises, and the type each one has, all
  // the way down. A type may carry the suffix `-or-null` for a field the
  // engine declares nullable; nothing here is optional, and a *missing* field
  // is never the same thing as a null one.
  //
  // `{"protocol_version":1,"ready":true}` parses, announces a protocol this
  // plugin speaks, and is not a status report: it has no `snapshot`, no
  // `sessions`, no `capture`. Published, it renders as "no snapshot has been
  // recorded" and an empty session list — a machine with nothing to restore.
  // The engine never said that. An absent field is unknown, and unknown
  // rendered as no is the failure this whole plugin is built around, so a
  // response missing any of these is not a status at all.
  //
  // The nested objects need the same treatment for the same reason, and used
  // not to get it: `"capture": {}` satisfied a check that asked only whether
  // `capture` was an object, and `Menu.qml` then read the absent
  // `capture.stale` as "captures are fresh" and the absent
  // `capture.consecutive_failures` as zero. A machine whose captures had been
  // failing for a week rendered as a healthy one. So every field of every
  // nested object is declared here too.
  readonly property var requiredFields: ({
    "engine_version": "string",
    "ready": "boolean",
    "capture": "object",
    "database": "object",
    "tmux": "object",
    "agents": "object",
    "sessions": "array",
    "snapshot": "object-or-null"
  })

  // Fields the engine may leave out entirely, with the type they have when it
  // does not. `message` is the only one: a ready engine with nothing to say
  // omits it, and refusing that response would black out every healthy
  // machine. Present, it is rendered, so present-and-wrong is still a fault.
  readonly property var optionalFields: ({
    "message": "string"
  })

  readonly property var captureFields: ({
    "last_success_at": "number-or-null",
    "age_secs": "number-or-null",
    "stale": "boolean",
    "stale_after_secs": "number",
    "last_error": "string-or-null",
    "last_error_at": "number-or-null",
    "consecutive_failures": "number"
  })

  readonly property var databaseFields: ({
    "path": "string",
    "reachable": "boolean",
    "error": "string-or-null",
    "snapshots": "number-or-null",
    "newest_snapshot_at": "number-or-null",
    "preserved": "object-or-null"
  })

  // `present`, `snapshots` and `error` are what is actually in the preserved
  // file, read from it rather than assumed: a count means it was read and
  // holds that many snapshots (`0` is an answer — nothing of the user's is in
  // there), a null count beside an `error` means nothing could be read, and
  // `present: false` means it is not there any more. The engine used to
  // declare the snapshots in it intact without opening it, which on a backup
  // holding nothing told its owner he had lost work he never had.
  readonly property var preservedFields: ({
    "path": "string",
    "schema_version": "number-or-null",
    "preserved_at": "number-or-null",
    "present": "boolean",
    "snapshots": "number-or-null",
    "error": "string-or-null"
  })

  readonly property var tmuxFields: ({
    "socket": "string-or-null",
    "reachable": "boolean",
    "error": "string-or-null"
  })

  readonly property var agentsFields: ({
    "enabled": "array",
    "unsupported": "array"
  })

  readonly property var unsupportedAgentFields: ({
    "kind": "string",
    "reason": "string"
  })

  // `workspace` and `monitor` are a string or null and never absent: null is
  // the recorded fact "no window was seen for this session", and a missing key
  // is a different thing that must not be read as it.
  readonly property var sessionFields: ({
    "name": "string",
    "windows": "number",
    "panes": "number",
    "agents": "number",
    "workspace": "string-or-null",
    "monitor": "string-or-null",
    "goal": "object-or-null",
    "conversations": "array"
  })

  // A session's goal is one real title with the conversation it came from
  // attached, and every part of it is load-bearing. Without `kind` and
  // `native_id` the line could be drawn against the wrong conversation;
  // without `source` a line osm derived from the user's first prompt would be
  // indistinguishable from one the agent wrote about itself. Half a goal is
  // not a smaller goal, it is a claim nobody can check.
  readonly property var goalFields: ({
    "title": "string",
    "source": "string",
    "kind": "string",
    "native_id": "string"
  })

  // `title` and `title_source` are a string or null and never absent: null is
  // the recorded fact "osm could derive no title for this conversation", which
  // the menu draws as *untitled*, and a missing key is a different thing that
  // must not be read as it.
  readonly property var conversationFields: ({
    "kind": "string",
    "native_id": "string",
    "title": "string-or-null",
    "title_source": "string-or-null",
    "window_idx": "number",
    "pane_idx": "number",
    "last_active": "number-or-null"
  })

  readonly property var snapshotFields: ({
    "id": "number",
    "taken_at": "number",
    "age_secs": "number",
    "state": "string",
    "sessions": "number"
  })

  // `typeof` calls null an object and an array an object; neither is useful
  // here, and both are shapes that have to be told apart from a real one.
  function typeName(value) {
    if (value === null) return "null"
    if (value === undefined) return "missing"
    if (Array.isArray(value)) return "array"
    return typeof value
  }

  // Why this response is not one this plugin can read at all, or "".
  function readableFault(value) {
    if (typeName(value) !== "object") return "the response is " + typeName(value) + ", not a JSON object"
    if (typeof value.protocol_version !== "number") return "the response has no protocol_version"
    return ""
  }

  // Why this response's protocol is not this plugin's, or "".
  function protocolFault(value) {
    return Number(value.protocol_version) === supportedProtocol
      ? ""
      : "engine protocol " + String(value.protocol_version) + ", this plugin speaks " + supportedProtocol
  }

  // Why `value` does not match `contract`, or "".
  //
  // One function for every level of the response, so a nested object is held
  // to its fields exactly as strictly as the top level is to its own — the
  // asymmetry between the two is what let `"capture": {}` through.
  //
  // `optional` names the keys that may be absent. Absent, they are skipped;
  // present, they are checked, because the menu renders whatever is there.
  function fieldsFault(value, contract, optional) {
    if (typeName(value) !== "object") return "is " + typeName(value) + ", not an object"
    var names = Object.keys(contract)
    for (var i = 0; i < names.length; i++) {
      var name = names[i]
      var want = contract[name]
      var got = typeName(value[name])
      var nullable = want.length > 8 && want.substring(want.length - 8) === "-or-null"
      if (nullable) want = want.substring(0, want.length - 8)
      if (got === want) continue
      if (nullable && got === "null") continue
      if (optional && optional[name] !== undefined && got === "missing") continue
      return name + " is " + got + ", not " + want + (nullable ? " or null" : "")
    }
    return ""
  }

  // Why this response is not a complete protocol-1 status report, or "".
  function shapeFault(value) {
    var top = fieldsFault(value, root.requiredFields, null)
    if (top !== "") return top

    var optional = Object.keys(root.optionalFields)
    for (var o = 0; o < optional.length; o++) {
      var name = optional[o]
      if (value[name] === undefined) continue
      var want = root.optionalFields[name]
      var got = typeName(value[name])
      if (got !== want) return name + " is " + got + ", not " + want
    }

    var nested = [
      ["capture", root.captureFields],
      ["database", root.databaseFields],
      ["tmux", root.tmuxFields],
      ["agents", root.agentsFields]
    ]
    for (var n = 0; n < nested.length; n++) {
      var fault = fieldsFault(value[nested[n][0]], nested[n][1], null)
      if (fault !== "") return nested[n][0] + "." + fault
    }

    if (value.database.preserved !== null) {
      var pres = fieldsFault(value.database.preserved, root.preservedFields, null)
      if (pres !== "") return "database.preserved." + pres
    }

    // `enabled` is the list of agent kinds the menu prints verbatim; a
    // non-string in it renders as `[object Object]`.
    for (var e = 0; e < value.agents.enabled.length; e++) {
      if (typeName(value.agents.enabled[e]) !== "string")
        return "agents.enabled[" + e + "] is " + typeName(value.agents.enabled[e]) + ", not string"
    }
    for (var u = 0; u < value.agents.unsupported.length; u++) {
      var un = fieldsFault(value.agents.unsupported[u], root.unsupportedAgentFields, null)
      if (un !== "") return "agents.unsupported[" + u + "]." + un
    }

    for (var j = 0; j < value.sessions.length; j++) {
      var row = sessionFault(value.sessions[j])
      if (row !== "") return "sessions[" + j + "]: " + row
    }
    if (value.snapshot !== null) {
      var snap = snapshotFault(value.snapshot)
      if (snap !== "") return "snapshot: " + snap
    }
    return ""
  }

  function sessionFault(row) {
    var fault = fieldsFault(row, root.sessionFields, null)
    if (fault !== "") return fault
    if (row.goal !== null) {
      var goal = fieldsFault(row.goal, root.goalFields, null)
      if (goal !== "") return "goal." + goal
    }
    for (var i = 0; i < row.conversations.length; i++) {
      var one = fieldsFault(row.conversations[i], root.conversationFields, null)
      if (one !== "") return "conversations[" + i + "]." + one
    }
    return ""
  }

  function snapshotFault(snap) {
    return fieldsFault(snap, root.snapshotFields, null)
  }

  // Drop an answer that cannot still be current.
  //
  // Called before every probe and after every failure. One poll plus one
  // timeout is the longest a live loop can go without a new answer, so
  // anything older than that was produced by a loop that stopped running —
  // and a session count nobody has checked since is not a session count.
  function dropStaleAnswer() {
    if (lastProbeAt === 0) return
    if (Date.now() - lastProbeAt <= probeIntervalMs + probeTimeoutMs) return
    status = null
    protocolSeen = ""
    engineState = "unknown"
    detail = "the last answer was older than one poll and one timeout, and was dropped"
  }

  function elide(text, limit) {
    var value = String(text || "").replace(/\s+/g, " ").trim()
    var cap = limit || 400
    return value.length > cap ? value.substring(0, cap - 1) + "…" : value
  }

  function refresh() {
    dropStaleAnswer()
    if (statusProcess.running) return
    statusTimedOut = false
    statusProcess.command = root.argv(["status", "--json"])
    statusProcess.running = true
    statusProcessWatchdog.restart()
  }

  // Nothing survived this probe: forget the last answer and say why.
  //
  // Clearing is the point. A session list, a snapshot id and a conversation
  // count all describe a moment that has passed; left beside an error message
  // they read as current, and the user is looking at a machine's state from
  // some earlier time with no way to tell.
  function failProbe(state, why) {
    status = null
    protocolSeen = ""
    failures += 1
    engineState = state
    detail = why
  }

  // One probe's verdict, in the order the states must be checked. Each branch
  // records the evidence it decided on, so the menu can show it.
  function applyProbe(exitCode, stdout, stderr) {
    lastProbeAt = Date.now()

    var parsed = null
    try {
      parsed = JSON.parse(String(stdout || ""))
    } catch (e) {
      parsed = null
    }

    // Parsed first, exit code second. `osm status --json` prints its JSON and
    // reports the refusal *inside* it — an unsupported tmux is a status
    // report, not an absent engine — so a non-zero exit with good JSON is
    // still an answer.
    var unreadable = parsed === null ? "the output was not JSON" : readableFault(parsed)
    if (unreadable !== "") {
      if (exitCode === 127 || exitCode === 126) {
        var missing = exitCode === 126
          ? root.osmPath + " is not executable"
          : root.osmPath + " was not found on PATH"
        if (String(stderr || "").trim() !== "") missing += " (" + elide(stderr, 200) + ")"
        failProbe("missing", missing)
        return
      }
      var said = elide(stderr || stdout || "", 400)
      failProbe("unreadable", said === ""
        ? unreadable + "; the probe exited " + exitCode + " and said nothing"
        : unreadable + "; " + said)
      return
    }

    // The version is readable on any response that got this far, and it is
    // the only field of an unrecognised one that may be.
    protocolSeen = String(parsed.protocol_version)

    // Protocol before shape. A different major has a different shape, so
    // measuring it against this plugin's fields would report a missing field
    // when the truth is a version this code has never seen.
    var wrongProtocol = protocolFault(parsed)
    if (wrongProtocol !== "") {
      status = null
      failures += 1
      engineState = "incompatible"
      detail = wrongProtocol
      return
    }

    // Shape before publication. Everything below this line renders from
    // `status`, and a half-shaped object renders as an absence.
    var wrongShape = shapeFault(parsed)
    if (wrongShape !== "") {
      failProbe("unreadable", "the response is not a complete status report: " + wrongShape)
      return
    }

    failures = 0
    status = parsed
    detail = elide(stderr, 400)
    engineState = parsed.ready === true ? "ready" : "notReady"
  }

  // ---- Menu wiring. Menu.qml is loaded here, by this Loader, and is
  //      deliberately not a declared kind in manifest.json: a second kind
  //      would make the shell mount it in its own right, so the menu would
  //      exist twice and poll twice.
  function injectMenu() {
    var target = menuLoader.item
    if (!target) return
    if ("bar" in target) target.bar = root.bar
    if ("settings" in target) target.settings = root.settings
    if ("anchorItem" in target) target.anchorItem = button
    if ("hostWidget" in target) target.hostWidget = root
    if ("widget" in target) target.widget = root
  }

  // Shape contract for shell.summon/hide/toggle routing: Bar.findPanelWidget
  // requires open/close/opened on the bar-widget root.
  readonly property bool opened: menuLoader.item ? menuLoader.item.opened === true : false

  function open() { if (menuLoader.item) menuLoader.item.open() }
  function close() { if (menuLoader.item) menuLoader.item.close() }
  function togglePanel() { if (menuLoader.item) menuLoader.item.toggle() }

  readonly property bool popoutSwitchClosing: menuLoader.item ? menuLoader.item.popoutSwitchClosing === true : false
  function closeForPopoutSwitch() { if (menuLoader.item) menuLoader.item.closeForPopoutSwitch() }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onBarChanged: injectMenu()
  onSettingsChanged: injectMenu()

  Loader {
    id: menuLoader
    active: true
    source: Qt.resolvedUrl("Menu.qml")
    visible: false
    onLoaded: {
      root.injectMenu()
      Qt.callLater(root.injectMenu)
    }
  }

  Timer {
    id: pollTimer
    interval: root.probeIntervalMs
    repeat: true
    running: true
    triggeredOnStart: true
    onTriggered: root.refresh()
  }

  Process {
    id: statusProcess
    running: false
    command: []
    stdout: StdioCollector { id: statusOut; waitForEnd: true }
    stderr: StdioCollector { id: statusErr; waitForEnd: true }
    onExited: function (exitCode) {
      statusProcessWatchdog.stop()
      // The exit that follows a kill describes the kill. The watchdog has
      // already published what it knows, and this is not new evidence.
      if (root.statusTimedOut) return
      root.applyProbe(exitCode, String(statusOut.text || ""), String(statusErr.text || ""))
    }
  }

  // The bound on `statusProcess`. Without it a probe that never returns leaves
  // the last good answer on the bar for as long as the shell runs.
  Timer {
    id: statusProcessWatchdog
    interval: root.probeTimeoutMs
    repeat: false
    onTriggered: {
      root.statusTimedOut = true
      root.lastProbeAt = Date.now()
      root.failProbe("unreadable", "the engine did not answer within "
        + Math.round(root.probeTimeoutMs / 1000) + "s and was stopped")
      // SIGTERM, then release the handle: a process the shell has stopped
      // waiting for must also stop being one it will not start another for.
      statusProcess.signal(15)
      statusProcess.running = false
    }
  }

  IpcHandler {
    target: "io.github.n8group-oss.sessionmemory"

    function refresh(): void { root.broadcast("refresh") }
    function open(): void { root.open() }
    function close(): void { root.close() }
    function show(): void { root.open() }
    function hide(): void { root.close() }
    function toggle(): void { root.togglePanel() }
    function state(): string { return root.engineState }
  }

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.barLabel
    tooltipText: root.summary
    // Dimmed for every state that is not a working engine, and urgent for the
    // two that are a problem rather than an absence. The bar has to be
    // readable at a glance, and "installed but doing nothing" must not look
    // the same as "installed and idle".
    dimmed: root.engineState === "unknown" || root.engineState === "missing"
    active: root.engineState === "notReady" || root.engineState === "incompatible" || root.engineState === "unreadable"
    horizontalMargin: 8.0

    onPressed: function (b) {
      if (b === Qt.MiddleButton) root.refresh()
      else root.togglePanel()
    }
  }
}
