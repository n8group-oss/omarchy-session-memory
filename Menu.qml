import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

// The popup half of the plugin. BarWidget.qml owns the status probe and loads
// this file with a Loader; this file renders what the probe found and runs
// the two engine commands the footer offers.
//
// What it renders is the *recorded* state — the newest snapshot — not the
// live tmux server. That is why there is no per-session focus or kill button
// here: `status.sessions` is what osm would restore, and a session in that
// list may have been closed since. A button that killed "the session called
// dev" would be acting on a name, on a server this plugin never looked at,
// with no way to confirm it hit the thing on screen. The engine has no
// command for either action, and inventing one in QML would put the decision
// in the layer least able to verify it.
//
// What is shown of a conversation is its kind, its id, its project directory
// and one short line saying what it is about — a title. Nothing else out of a
// transcript is read or drawn: no messages, no bodies, no excerpts. Where that
// line was derived from the user's first prompt rather than written by the
// agent, the detail behind a session row says so, because "the agent called
// this X" and "the person opened by asking X" are different claims. See
// `docs/design.md` and `src/agent/title.rs`.
//
// Every row of more than one thing in here is a RowLayout or a Flow, never a
// plain Row. A Row is a positioner, not a layout: it places each child at that
// child's own width, left to right, and never once looks at how much room the
// panel has — so a 36-character conversation id beside a 60-character project
// path pushes the "Copy resume" button past the right edge, where the
// Flickable clips it. That is what the maintainer saw.
//
// The rule these rows follow has three parts, and each one is a `Layout`
// attached property because that is the only mechanism that actually looks at
// the available width:
//
//   * The elastic child — the one holding unbounded user data, a project path
//     or a session name — asks for no width of its own
//     (`Layout.preferredWidth: 0`) and takes the leftover
//     (`horizontalStretchFactor: 1`), eliding when the leftover is small.
//   * Every other label keeps its natural width but is still `fillWidth` with
//     `horizontalStretchFactor: 0` — a QtQuick layout only ever resizes items
//     that fill, so a label that does not fill cannot shrink, and a row of
//     labels that cannot shrink overflows exactly like a `Row` would. Stretch
//     zero means it never grows past its text either.
//   * A control that is unusable half-drawn does not fill at all and declares
//     `Layout.minimumWidth: <its own implicitWidth>`, so it is fixed at its
//     label and every pixel of pressure lands on the labels instead.
//
// Rows that are nothing but buttons wrap instead of running off the edge.
Panel {
  id: root
  moduleName: "io.github.n8group-oss.sessionmemory"
  ipcTarget: "io.github.n8group-oss.sessionmemory"
  manageIpc: false

  property var anchorItem: null

  // The bar tracks the widget mounted in its slot, not this nested panel, so
  // everything the bar identifies a panel by has to be that widget.
  property var hostWidget: null
  readonly property var barIdentity: hostWidget || root

  // The BarWidget. Every status fact below is read through it, so there is
  // exactly one probe and one truth.
  property var widget: null

  readonly property string engineState: widget ? String(widget.engineState) : "unknown"
  readonly property var status: widget ? widget.status : null
  readonly property string detail: widget ? String(widget.detail) : ""
  readonly property int supportedProtocol: widget ? widget.supportedProtocol : 1
  readonly property string osmPath: widget ? String(widget.osmPath) : "osm"
  readonly property int probeTimeoutMs: widget ? widget.probeTimeoutMs : 10000
  readonly property string protocolFound: widget && typeof widget.protocolFound === "function"
    ? String(widget.protocolFound())
    : "unknown"

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color urgent: bar ? bar.urgent : Color.urgent
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  // ---- Actions. Same argv discipline as the widget: an array, always.
  function argv(args) {
    return ["/usr/bin/env", root.osmPath].concat(args)
  }

  property string actionStatus: ""
  property bool restoreArmed: false
  // Which command `actionProcess` is currently running, so its output is read
  // as what it is: `osm restore --json` prints a report worth reading and
  // `osm snapshot` prints nothing of the kind.
  property string actionKind: ""

  // The agents probe. Deliberately not on a timer: it walks /proc and every
  // agent's transcript store, which is far too much work to repeat every five
  // seconds for a popup nobody has open. It runs when the menu opens, and
  // when asked.
  property var agents: null
  property string agentsError: ""
  property double agentsAt: 0

  readonly property var liveAgents: (agents && agents.live) ? agents.live : []
  readonly property var resumableAgents: (agents && agents.resumable) ? agents.resumable : []
  readonly property var agentProblems: (agents && agents.problems) ? agents.problems : []

  // How many resumable conversations this menu draws.
  //
  // The engine answers with all of them, and it is right to: `osm agents
  // --json` is an inventory and its other readers want the whole thing. What
  // arrives on this machine is 2334 conversations going back seven months.
  // The menu rendered every one — a `Repeater` with 2334 delegates, each a
  // row of two labels and a button — which is slow to build and, long before
  // that matters, is not a list a person can use.
  //
  // Twenty-five, because the shape of the data says so. Two counts of this
  // machine's store put a day's conversations at 15 and at 23, a week's at 56
  // and at 67, a month's in the hundreds; the rest — 1834 of them — are older
  // than a month. Twenty-five covers the busier of those two days outright,
  // which is the case this list exists for: a reboot interrupted something
  // and the user wants it back. Past a day it is the count beside the list,
  // not the list, that carries the truth — so a larger cap buys scrolling
  // rather than answers, and a smaller one starts cutting into today.
  //
  // The panel already scrolls, so the cap is not about fitting on screen. It
  // is about how many delegates get built while the popup opens, and 25 is a
  // hundredth of what was being built.
  readonly property int resumableCap: 25

  // The least width an identifying field is ever drawn in: roughly a dozen
  // characters at the panel's body size.
  //
  // Every row here has one field that says *which* session or *which*
  // conversation it is, and metadata beside it that is often identical from
  // row to row. A field declared `Layout.fillWidth: true` with no minimum is
  // one Qt may shrink to nothing — a stretch factor decides who gets spare
  // room, and under width pressure there is none — and a panel of rows
  // showing counts, a monitor and no name is worse than one that overflows:
  // the user cannot tell the rows apart at all. So the identity keeps a
  // floor, and a row that cannot honour it on one line stacks instead of
  // squeezing.
  readonly property int identityFloor: Style.space(96)

  // The conversations the filter box lets through, and then the list as it is
  // drawn: those matches, newest first, cut to the cap. The cap is applied to
  // the matches and not to the whole store, so typing is how the user reaches
  // something older than the newest 25.
  readonly property var matchingResumableAgents: root.conversationsMatching(root.resumableAgents, root.filterText)
  readonly property var shownResumableAgents: root.newestFirst(root.matchingResumableAgents, root.resumableCap)

  // When a conversation was last active, as a number that sorts.
  //
  // A row with no `last_active` — the field is optional, and an agent whose
  // store could not be read fully may omit it — is not "just now" and is not
  // zero either. It sorts *below* every conversation whose age is known,
  // because an unknown age must never displace a known recent one out of a
  // capped list.
  function lastActive(row) {
    var n = Number(row && row.last_active)
    return isFinite(n) ? n : Number.NEGATIVE_INFINITY
  }

  // `rows` newest first, cut to `cap` (`cap <= 0` cuts nothing).
  //
  // Recency and nothing else. Ordering by, say, the current project first
  // would push older work up the list and therefore push *newer* work off
  // the end of it, which is the one thing a cap must not do — and this menu
  // has no honest notion of a current project anyway: it is a bar popup, not
  // something attached to a pane.
  //
  // Ties break on `native_id` so the order is the same on every refresh
  // rather than resting on whether the engine's sort happens to be stable.
  // The input is never mutated: it is a bound property, and sorting it in
  // place would reorder what every other reader of it sees.
  function newestFirst(rows, cap) {
    var out = []
    for (var i = 0; i < rows.length; i++) out.push(rows[i])
    out.sort(function (a, b) {
      var x = root.lastActive(a)
      var y = root.lastActive(b)
      if (x !== y) return y > x ? 1 : -1
      var ka = String(a && a.native_id)
      var kb = String(b && b.native_id)
      return ka < kb ? -1 : (ka > kb ? 1 : 0)
    })
    return (cap > 0 && out.length > cap) ? out.slice(0, cap) : out
  }

  // ---- The filter.
  //
  // 2447 resumable conversations and seven recorded sessions arrive here, and
  // the list draws the newest 25 of them. That is honest and it is not
  // enough: the conversation a reboot interrupted is very often not among the
  // newest 25 — it is the one from last Thursday — and with no way to ask for
  // it the only route to an older conversation is to stop using this menu.
  // The maintainer asked for exactly this.
  //
  // The cap applies *after* the filter, which is the whole point: a search is
  // how someone reaches past the newest 25.
  //
  // Case-insensitive substring, and deliberately nothing more. Nobody typing
  // into a one-line box on a bar popup means `.` as "any character", and a box
  // that is quietly a pattern language hides rows for reasons its user cannot
  // see. Nothing here starts a process either: every row it matches against is
  // already in hand, so the list narrows on the keystroke and the engine is
  // never asked again.
  property string filterText: ""

  function normalizedQuery(value) {
    if (value === undefined || value === null) return ""
    return String(value).trim().toLowerCase()
  }

  // `query` is already normalized. An empty one matches everything, which is
  // what makes an empty box mean "no filter" rather than "match nothing".
  function containsQuery(haystack, query) {
    if (query === "") return true
    if (haystack === undefined || haystack === null) return false
    return String(haystack).toLowerCase().indexOf(query) !== -1
  }

  // A conversation is matched on what its own row puts on screen — what it is
  // about, the project directory, the kind, the id — and on nothing else.
  // Matching a field the user cannot see would make rows appear and vanish
  // for invisible reasons.
  //
  // The title is first because it is the useful one. A path narrows 2495
  // conversations to the hundreds in one project; after that the only thing
  // telling them apart is what each was for.
  function conversationMatches(row, query) {
    if (query === "") return true
    if (!row) return false
    return root.containsQuery(row.title, query)
      || root.containsQuery(row.project_dir, query)
      || root.containsQuery(row.kind, query)
      || root.containsQuery(row.native_id, query)
  }

  // The same rule for a session: its name, the goal on its row, and the
  // titles of the conversations behind that goal — which are on screen the
  // moment the row is opened.
  //
  // The name alone is not enough to search on and that is the whole point of
  // this change: `dev`, `scratch`, `ops` say nothing, and on a machine with
  // eight of them what tells two apart is what each is about.
  function sessionMatches(session, query) {
    if (query === "") return true
    if (!session) return false
    if (root.containsQuery(session.name, query)) return true
    if (session.goal && root.containsQuery(session.goal.title, query)) return true
    var rows = session.conversations ? session.conversations : []
    for (var i = 0; i < rows.length; i++) {
      if (root.containsQuery(rows[i].title, query)) return true
    }
    return false
  }

  // The matches, in the order they arrived. Filtering selects and does not
  // reorder: `newestFirst` runs after this, over the matches, so a search
  // answers with the newest conversations that match rather than the first
  // ones the engine happened to list. The input is copied, never filtered in
  // place — it is a bound property that other readers share.
  function conversationsMatching(rows, filter) {
    var query = root.normalizedQuery(filter)
    var out = []
    for (var i = 0; i < rows.length; i++) {
      if (root.conversationMatches(rows[i], query)) out.push(rows[i])
    }
    return out
  }

  function sessionsMatching(rows, filter) {
    var query = root.normalizedQuery(filter)
    var out = []
    for (var i = 0; i < rows.length; i++) {
      if (root.sessionMatches(rows[i], query)) out.push(rows[i])
    }
    return out
  }

  // What the drawn list says about itself.
  //
  // A menu that shows twenty of 2334 without saying so is the same defect as
  // an unknown rendered as a no: the user reads a list, believes it is the
  // list, and concludes that work they have not finished is gone. The count
  // that exists is stated first, and the cap second, so the number that is
  // true about the machine is the one they read.
  //
  // A filter adds a second way to be misread, and now three numbers have to be
  // reconciled: how many conversations exist, how many match what was typed,
  // and how many are drawn. The rule is that the user can always tell which of
  // three things happened — there are none; none match what you typed; there
  // are more matches than are shown — so the count that exists is stated in
  // every one of them, the match count whenever a filter is in force, and the
  // drawn count only when it is smaller than the matches.
  //
  // With the box empty the sentence is word for word the one that was here
  // before the box existed. A filter is an addition; it must not change what
  // the menu says to someone who never uses it. Whitespace is not a query, so
  // a stray space is an empty box and not a search that matched nothing.
  function resumableNote(total, matching, shown, filter) {
    var t = Number(total)
    var m = Number(matching)
    var s = Number(shown)
    var query = root.normalizedQuery(filter)
    if (!isFinite(t) || !isFinite(m) || !isFinite(s)) return "unknown"
    if (t === 0) return "Nothing waiting to be resumed."
    if (query === "") {
      var head = root.countText(t, "conversation", "conversations") + " waiting to be resumed"
      if (s >= t) return head + "."
      return head + " — showing the " + s + " most recently active."
    }
    var quoted = "“" + String(filter).trim() + "”"
    if (m === 0)
      return "No conversation matches " + quoted + " — "
        + root.countText(t, "conversation", "conversations") + " waiting to be resumed."
    var matched = m + " of " + root.countText(t, "conversation", "conversations")
      + (m === 1 ? " matches " : " match ") + quoted
    if (s >= m) return matched + "."
    return matched + " — showing the " + s + " most recently active."
  }

  // The same line for the session list, which has no cap — every match is
  // drawn, so there are two numbers here and not three.
  //
  // It appears only while a filter is in force. With the box empty this
  // section says exactly what it said before, and a line restating a list the
  // user can already see in full would be noise.
  function sessionsNote(total, matching, filter) {
    var query = root.normalizedQuery(filter)
    if (query === "") return ""
    var t = Number(total)
    var m = Number(matching)
    if (!isFinite(t) || !isFinite(m)) return "unknown"
    if (t === 0) return ""
    var quoted = "“" + String(filter).trim() + "”"
    if (m === 0)
      return "No session matches " + quoted + " — "
        + root.countText(t, "session", "sessions") + " recorded."
    return m + " of " + root.countText(t, "session", "sessions")
      + (m === 1 ? " matches " : " match ") + quoted + "."
  }

  readonly property var sessions: (status && status.sessions) ? status.sessions : []
  readonly property var snapshot: (status && status.snapshot) ? status.snapshot : null

  // The sessions the filter box lets through. A workspace group whose rows all
  // failed the filter has nothing left to head and goes with them.
  readonly property var matchingSessions: root.sessionsMatching(root.sessions, root.filterText)

  // Sessions grouped by the workspace their terminal window was recorded on.
  //
  // A session with no recorded placement goes into its own group with a
  // label that says *unknown*, never a default workspace: "no window was
  // recorded for this session" and "this session was on workspace 1" are
  // different facts and only one of them is ever true.
  readonly property var groups: {
    var order = []
    var byKey = ({})
    var rows = root.matchingSessions
    for (var i = 0; i < rows.length; i++) {
      var s = rows[i]
      var known = s.workspace !== undefined && s.workspace !== null && String(s.workspace) !== ""
      var key = known ? "ws:" + String(s.workspace) : "unknown"
      if (!byKey[key]) {
        byKey[key] = { title: known ? "WORKSPACE " + String(s.workspace) : "NO WINDOW RECORDED", rows: [] }
        order.push(key)
      }
      byKey[key].rows.push(s)
    }
    var out = []
    for (var j = 0; j < order.length; j++) out.push(byKey[order[j]])
    return out
  }

  function elide(text, limit) {
    var value = String(text || "").replace(/\s+/g, " ").trim()
    var cap = limit || 300
    return value.length > cap ? value.substring(0, cap - 1) + "…" : value
  }

  function ago(seconds) {
    var n = Number(seconds)
    if (!isFinite(n) || n < 0) return "unknown"
    if (n < 60) return Math.round(n) + "s ago"
    if (n < 3600) return Math.round(n / 60) + "m ago"
    if (n < 86400) return Math.round(n / 3600) + "h ago"
    return Math.round(n / 86400) + "d ago"
  }

  function countText(n, one, many) {
    var v = Number(n)
    if (!isFinite(v)) return "unknown"
    return v + " " + (v === 1 ? one : many)
  }

  // ---- The capture line, and the one thing it must never do: present a
  //      resolved failure as a condition.
  //
  //      `last_error` is a *record*, not a state. `record_success` in
  //      `src/health.rs` clears the failure streak and deliberately leaves the
  //      error text and its timestamp in place, so that an operator can still
  //      read what went wrong. Deciding whether that record describes right
  //      now is this panel's job. It used to append it unconditionally, and
  //      the maintainer's bar read "captures are fresh · the window placement
  //      for this capture could not be read …" — an error 1.7 hours old, with
  //      `last_success_at` 14 seconds ago and no failing streak, drawn as
  //      though it were happening. So the two stamps are compared, and each
  //      answer gets its own sentence.

  // A status field as a finite number, or `null`.
  //
  // `undefined`, `null` and anything unparsable all come back as `null` rather
  // than as `0`: `Number(null)` is 0, which would date an undated error to
  // 1970 and call it ancient, which is the same lie in the other direction.
  function captureTime(value) {
    if (value === undefined || value === null) return null
    var n = Number(value)
    return isFinite(n) ? n : null
  }

  // How many seconds ago the error happened, or `null` when that cannot be
  // worked out from what the engine sent.
  //
  // There is no clock in this report and none is invented here. `age_secs` is
  // the only field that ties an epoch stamp to now, and it is the age of
  // `last_success_at` — so now is `last_success_at + age_secs`, and the
  // error's age follows from that. Without a success there is no now. A
  // negative answer means the error is stamped in the future, which is not an
  // age; it is reported as unknown rather than rounded to something confident.
  function captureErrorAge(capture) {
    if (!capture) return null
    var at = root.captureTime(capture.last_error_at)
    var ok = root.captureTime(capture.last_success_at)
    var age = root.captureTime(capture.age_secs)
    if (at === null || ok === null || age === null) return null
    var seconds = age + (ok - at)
    return seconds >= 0 ? seconds : null
  }

  // Whether the recorded error is a thing that is wrong now.
  //
  // The failure streak is the authority, and the timestamps only corroborate
  // it. `record_success` in `src/health.rs` clears `consecutive_failures`, so
  // a non-zero one means every capture since the last success has failed and
  // the recorded error is the current condition — there is nothing left for
  // the stamps to decide.
  //
  // They cannot always decide it anyway. `last_success_at` and
  // `last_error_at` are epoch *seconds*, and a capture that succeeds and then
  // fails inside the same second leaves them equal: `at > ok` is false, and
  // the line drew a failure that is happening right now as "past error … with
  // a successful capture since". On this machine a capture triggers the tmux
  // hook that triggers the next one, so a success and a failure in one second
  // is an ordinary sequence.
  //
  // With no streak the stamps are all there is: nothing has succeeded since
  // the error either because the error is newer, or because there has never
  // been a success at all. An error with no timestamp and no streak answers
  // false, because "it is happening now" is a claim, and an undated record
  // does not support it; the sentence below says the time is unknown instead.
  function captureErrorIsCurrent(capture) {
    if (!capture || !capture.last_error) return false
    if (Number(capture.consecutive_failures) > 0) return true
    var at = root.captureTime(capture.last_error_at)
    if (at === null) return false
    var ok = root.captureTime(capture.last_success_at)
    return ok === null || at > ok
  }

  // The error clause, or "" when there is no error to report.
  //
  // Each branch says exactly what this report supports and no more. "Newer
  // than the last success" is a claim about the two stamps and is only made
  // when they order that way; where the streak is what establishes the error
  // is current, the sentence says so instead of borrowing an ordering that
  // is not there.
  function captureErrorNote(capture) {
    if (!capture || !capture.last_error) return ""
    var text = root.elide(capture.last_error, 200)
    var at = root.captureTime(capture.last_error_at)
    var ok = root.captureTime(capture.last_success_at)
    var age = root.captureErrorAge(capture)
    var when = age === null ? "" : ", " + root.ago(age)
    if (root.captureErrorIsCurrent(capture)) {
      if (ok === null)
        return "current error, and no capture has ever succeeded: " + text
      if (at !== null && at > ok)
        return "current error" + when + ", newer than the last success: " + text
      return "current error" + when + ", with captures failing since the last success: " + text
    }
    if (at === null)
      return "error of unknown time, so whether it is current is not known: " + text
    return "past error" + when + ", with a successful capture since: " + text
  }

  function captureLine(capture) {
    if (!capture) return ""
    var parts = []
    parts.push(capture.stale === true ? "captures are stale" : "captures are fresh")
    if (Number(capture.consecutive_failures) > 0)
      parts.push(capture.consecutive_failures + " failing in a row")
    var note = root.captureErrorNote(capture)
    if (note !== "") parts.push(note)
    return parts.join(" · ")
  }

  // The alarm colour follows the same determination as the words. A stale
  // record or a failing streak has always earned it; a *current* error earns
  // it too, and a resolved one does not — a line the user has to read twice to
  // find out it is about last Tuesday is the defect wearing a different coat.
  function captureIsAlarming(capture) {
    if (!capture) return false
    return capture.stale === true
      || Number(capture.consecutive_failures) > 0
      || root.captureErrorIsCurrent(capture)
  }

  // What the newest snapshot knows about where its sessions' terminal
  // windows were, or "" when there is nothing to say.
  //
  // A capture whose placement could not be read no longer fails — it records
  // the tmux topology, which is the part worth having — so `capture` reads
  // perfectly healthy while the snapshot beside it holds no window placement.
  // Freshness alone would then tell the user their state is fully recorded
  // when part of it is not, which is this plugin's own failure mode pointed
  // at their confidence.
  //
  // Three values, three sentences. `known` is a complete answer, empty desktop
  // or not, and says nothing. `disabled` is the user's own configuration and
  // is stated plainly. Only `unknown` is a shortfall.
  function placementNote(snap) {
    if (!snap) return ""
    var placement = String(snap.placement || "")
    if (placement === "unknown")
      return "this snapshot could not read where the windows were; a restore "
           + "will use the last layout this boot recorded, if there is one"
    if (placement === "disabled")
      return "window placement is switched off, so none was recorded"
    return ""
  }

  // And the alarm colour is reserved for the one of the three that is a
  // shortfall. Colouring "placement is switched off" as a problem reports a
  // failure the user themselves configured.
  function placementIsAlarming(snap) {
    return !!snap && String(snap.placement || "") === "unknown"
  }

  // A session row's placement line. Absent monitor reads "monitor unknown"
  // rather than being left blank, for the same reason as the group label.
  function placementText(session) {
    var monitor = session.monitor !== undefined && session.monitor !== null && String(session.monitor) !== ""
      ? String(session.monitor)
      : "monitor unknown"
    return monitor
  }

  // Which session row is showing its detail, by name. Empty for none.
  //
  // On the panel and not in the delegate: the status probe reruns on a timer
  // and rebuilds every delegate with it, so a row holding its own open flag
  // would close itself every few seconds while being read.
  property string expandedSession: ""

  function toggleSession(name) {
    var key = String(name)
    root.expandedSession = (root.expandedSession === key) ? "" : key
  }

  // What a session is about, in the words of the conversation that said so.
  //
  // One recorded title, chosen by the engine — the newest conversation in the
  // session that has one — and never a summary stitched out of several. A
  // session whose conversations osm could name nothing from reads *untitled*:
  // a blank line would say it has no purpose, and an invented one would be
  // worse still.
  function goalText(session) {
    var goal = session ? session.goal : null
    if (goal && goal.title) return String(goal.title)
    var rows = (session && session.conversations) ? session.conversations : []
    if (rows.length === 0) return "No conversation was recorded in this session."
    return "untitled"
  }

  function titleText(row) {
    return (row && row.title) ? String(row.title) : "untitled"
  }

  // Where a conversation was, and where its title came from.
  //
  // The provenance is here rather than on the row above because this is where
  // a reader has asked for the detail: a line osm derived from someone's first
  // prompt is not the same claim as one the agent wrote about itself, and the
  // difference is stated rather than left to be guessed.
  function paneText(row) {
    if (!row) return ""
    var place = "w" + String(row.window_idx) + ".p" + String(row.pane_idx)
    if (row.title_source === "first_prompt") return place + " \u00b7 from the first prompt"
    if (row.title_source === "agent") return place + " \u00b7 agent's own title"
    return place
  }

  function shortId(id) {
    var value = String(id || "")
    return value.length > 12 ? value.substring(0, 12) + "…" : value
  }

  // Set by a watchdog when it stops the process it bounds. The exit that
  // follows a kill describes the kill, not the engine.
  property bool agentsTimedOut: false
  property bool actionTimedOut: false

  function refreshAgents() {
    if (agentsProcess.running || engineState !== "ready") return
    agentsTimedOut = false
    agentsProcess.command = root.argv(["agents", "--json"])
    agentsProcess.running = true
    agentsProcessWatchdog.restart()
  }

  // Why this is not a conversation list this plugin can read, or "".
  //
  // The same rule as the status probe, and for the same reason: an answer
  // that merely *has* a protocol_version can be protocol 2, whose entries
  // have fields this code has never seen — and a list rendered from one is a
  // guess presented as an inventory. `live`, `resumable` and `problems` must
  // all be arrays, because a missing `problems` renders as "nothing could not
  // be read", which is the strongest claim of the three.
  function agentsFault(value) {
    if (root.typeName(value) !== "object") return "the response is " + root.typeName(value) + ", not a JSON object"
    if (typeof value.protocol_version !== "number") return "the response has no protocol_version"
    if (Number(value.protocol_version) !== root.supportedProtocol)
      return "engine protocol " + String(value.protocol_version) + ", this plugin speaks " + root.supportedProtocol
    var lists = ["live", "resumable", "problems"]
    for (var i = 0; i < lists.length; i++) {
      if (root.typeName(value[lists[i]]) !== "array") return lists[i] + " is " + root.typeName(value[lists[i]]) + ", not an array"
    }
    for (var j = 0; j < value.resumable.length; j++) {
      var row = value.resumable[j]
      if (root.typeName(row) !== "object") return "resumable[" + j + "] is " + root.typeName(row) + ", not an object"
      if (typeof row.kind !== "string") return "resumable[" + j + "] has no kind"
      if (typeof row.native_id !== "string") return "resumable[" + j + "] has no native_id"
    }
    return ""
  }

  // `typeof` calls null an object and an array an object. Borrowed from the
  // widget so both probes answer the same way, with a local fallback for the
  // moment before the widget is injected.
  function typeName(value) {
    if (widget && typeof widget.typeName === "function") return String(widget.typeName(value))
    if (value === null) return "null"
    if (value === undefined) return "missing"
    if (Array.isArray(value)) return "array"
    return typeof value
  }

  function refreshAll() {
    if (widget && typeof widget.refresh === "function") widget.refresh()
    refreshAgents()
  }

  function snapshotNow() {
    if (actionProcess.running || engineState !== "ready") return
    actionKind = "capture"
    actionStatus = "Capturing…"
    actionTimedOut = false
    actionProcess.command = root.argv(["snapshot", "--reason", "menu"])
    actionProcess.running = true
    actionProcessWatchdog.restart()
  }

  // Restore is the one action here that rebuilds a machine's session layout,
  // so it is two clicks: the first arms it and says what it will do, the
  // second runs it. A single-click restore on a bar popup is one misclick
  // away from a surprise.
  function restoreNow() {
    if (actionProcess.running || engineState !== "ready") return
    if (!restoreArmed) {
      restoreArmed = true
      actionStatus = "Restore rebuilds the sessions in snapshot "
        + (snapshot ? "#" + snapshot.id : "(none recorded)")
        + ". Click again to confirm."
      disarmTimer.restart()
      return
    }
    restoreArmed = false
    disarmTimer.stop()
    actionKind = "restore"
    actionStatus = "Restoring…"
    actionTimedOut = false
    actionProcess.command = root.argv(["restore", "--json"])
    actionProcess.running = true
    actionProcessWatchdog.restart()
  }

  // A value as one POSIX shell word.
  //
  // Everything else this plugin runs goes out as an argv array, where a
  // conversation id is one argument whatever is in it. The clipboard is the
  // one place that discipline ends: the text below is put in front of a person
  // and the menu tells them to paste it into a shell, so it *is* a command
  // line, and an id of `x; touch /tmp/pwn #` pasted verbatim is a command they
  // run against themselves. Conversation ids come from agent transcript stores
  // — file names on disk — and are not this plugin's to trust.
  //
  // Single quotes, because inside them a POSIX shell expands nothing at all:
  // no `$`, no backtick, no `$(…)`, no `;`, no `#`. The one character that can
  // end the quoting is a single quote, and it is closed, escaped and reopened
  // — `'` becomes `'\''` — which is the only escape this needs and the reason
  // this is four lines rather than a table of characters to strip.
  function shellQuote(value) {
    return "'" + String(value).split("'").join("'\\''") + "'"
  }

  function copyResumeCommand(entry) {
    if (!entry || !entry.native_id) return
    // Not executed — placed on the clipboard. `osm resume` resumes into the
    // pane it is run from ($TMUX_PANE), and a bar popup is not in a pane; a
    // button that ran it here would report pane_missing every time.
    //
    // `--` before the id as well as the quoting: an id beginning with `-` is
    // a value, and without the separator both the shell's `osm` and clap
    // would read it as a flag.
    Quickshell.clipboardText = "osm resume -- " + root.shellQuote(entry.native_id)
    actionStatus = "Resume command copied — run it in the pane you want it in."
    actionStatusTimer.restart()
  }

  // The `placement_carried` note in an `osm restore --json` report, or null.
  //
  // The engine has three ways of putting a window layout back and one of them
  // is *borrowed*: when the source snapshot's placement could not be read at
  // capture time, the restore carries the last layout the same boot knew and
  // says so, naming the snapshot it took it from. That report exists because
  // a restore that used a layout it did not itself record has to say so — and
  // this panel is the consumer it was written for.
  //
  // Every failure to read the output is `null`, never a throw: this same
  // `Process` also runs `osm snapshot`, which prints no report at all, and a
  // run the watchdog killed leaves whatever half a report it had written.
  function carriedNote(out) {
    var parsed = null
    try {
      parsed = JSON.parse(String(out || ""))
    } catch (e) {
      return null
    }
    if (!parsed || !parsed.windows || !parsed.windows.length) return null
    for (var i = 0; i < parsed.windows.length; i++) {
      var w = parsed.windows[i]
      if (w && String(w.outcome) === "placement_carried") return w
    }
    return null
  }

  // The snapshot a carried layout came from, or "" when the note does not
  // name one.
  //
  // Read out of the note's own sentence, which is where the engine puts it —
  // `windows[]` carries a session, an outcome and a detail, and the detail is
  // the only place the source id appears. `tests/qml_restore_line.rs` builds
  // its fixture from the engine's own `format!` so that a change to that
  // sentence fails there rather than quietly leaving this returning "".
  function carriedFrom(note) {
    if (!note || !note.detail) return ""
    var m = /snapshot ([0-9]+)/.exec(String(note.detail))
    return m ? m[1] : ""
  }

  // What a finished restore is called.
  //
  // `Done.` for a restore that replayed the layout it recorded itself. A
  // restore that borrowed one says so and names the snapshot it borrowed
  // from: the user's terminals are back, but on a layout from earlier in the
  // boot, and being told the same word as an exact replay is how a panel
  // hides the one thing about this run that was unusual.
  function restoreDone(out) {
    var note = root.carriedNote(out)
    if (note === null) return "Done."
    var from = root.carriedFrom(note)
    return from === ""
      ? "Done — this snapshot had no window layout recorded; an earlier one "
        + "from this boot was used."
      : "Done — this snapshot had no window layout recorded; the one from "
        + "snapshot #" + from + " was used."
  }

  // The released installer, not a build from git.
  //
  // Each `v*` tag publishes the binary, its SHA-256, and an `install.sh` with
  // *that build's* digest written into it by the workflow that built it. The
  // script checks the download against the digest it was built with rather
  // than against a checksum fetched from the same server as the file, which
  // would check for corruption and for nothing else.
  //
  // What this replaced was a one-liner that fetched the repository at whatever
  // state its branch happened to be in, built it with a full Rust toolchain,
  // and ran the result. The Omarchy marketplace's automated review reads a
  // line like that as `package-manager` plus `remote-build`, and it is right
  // to: it is slower, it needs far more installed, and it verifies nothing.
  // Building from source belongs in the README, where a developer will look
  // for it — and it stays the documented route on a machine this release has
  // no binary for.
  function copyInstallCommand() {
    Quickshell.clipboardText = "curl -fsSLO https://github.com/n8group-oss/omarchy-session-memory/releases/latest/download/install.sh && sh install.sh"
    actionStatus = "Install command copied — run `sh install.sh --dry-run` first to see every step."
    actionStatusTimer.restart()
  }

  onOpenedChanged: if (opened) {
    restoreArmed = false
    actionStatus = ""
    // A filter left over from last time would be a list that is short for a
    // reason nobody remembers typing.
    filterText = ""
    if (filterField) filterField.text = ""
    if (menuFlick) menuFlick.contentY = 0
    refreshAll()
    Qt.callLater(function () { keyCatcher.forceActiveFocus() })
  }

  Timer {
    id: actionStatusTimer
    interval: 4000
    repeat: false
    onTriggered: root.actionStatus = ""
  }

  Timer {
    id: disarmTimer
    interval: 6000
    repeat: false
    onTriggered: {
      root.restoreArmed = false
      root.actionStatus = ""
    }
  }

  Process {
    id: agentsProcess
    running: false
    command: []
    stdout: StdioCollector { id: agentsOut; waitForEnd: true }
    stderr: StdioCollector { id: agentsErr; waitForEnd: true }
    onExited: function (exitCode) {
      agentsProcessWatchdog.stop()
      if (root.agentsTimedOut) return
      var parsed = null
      try {
        parsed = JSON.parse(String(agentsOut.text || ""))
      } catch (e) {
        parsed = null
      }
      root.agentsAt = Date.now()
      var fault = parsed === null ? "the output was not JSON" : root.agentsFault(parsed)
      if (fault === "") {
        root.agents = parsed
        root.agentsError = ""
      } else {
        // The previous answer is dropped rather than left on screen looking
        // current: a stale conversation list is a list of things that may no
        // longer be resumable.
        root.agents = null
        var said = root.elide(String(agentsErr.text || ""), 200)
        root.agentsError = fault
          + (said === "" ? " (osm agents exited " + exitCode + ")" : " — " + said)
      }
    }
  }

  Timer {
    id: agentsProcessWatchdog
    interval: root.probeTimeoutMs
    repeat: false
    onTriggered: {
      root.agentsTimedOut = true
      root.agents = null
      root.agentsError = "osm agents did not answer within "
        + Math.round(root.probeTimeoutMs / 1000) + "s and was stopped"
      agentsProcess.signal(15)
      agentsProcess.running = false
    }
  }

  Process {
    id: actionProcess
    running: false
    command: []
    stdout: StdioCollector { id: actionOut; waitForEnd: true }
    stderr: StdioCollector { id: actionErr; waitForEnd: true }
    onExited: function (exitCode) {
      actionProcessWatchdog.stop()
      if (root.actionTimedOut) return
      var err = root.elide(String(actionErr.text || ""), 300)
      if (exitCode === 0) {
        // Not every exit 0 is the same sentence. `osm restore --json` prints
        // a report, and one of the things it reports is that the layout it
        // put back was borrowed from an earlier snapshot — which used to be
        // visible in the CLI and invisible here, in the one place it was
        // written for.
        root.actionStatus = root.actionKind === "restore"
          ? root.restoreDone(String(actionOut.text || ""))
          : "Done."
      } else {
        // The exit status is reported, not swallowed. `osm restore` exits
        // non-zero for a partial restore precisely so a caller can tell that
        // work is still owed.
        root.actionStatus = "Exited " + exitCode + (err === "" ? "" : ": " + err)
      }
      actionStatusTimer.restart()
      root.refreshAll()
    }
  }

  // Snapshot and restore are bounded too, and their bound is longer than the
  // probes': a restore rebuilds a whole session tree and legitimately takes
  // time. What it may not do is leave "Restoring…" on screen forever, with no
  // way to tell a long restore from a wedged one and both buttons disabled
  // behind `actionProcess.running`.
  Timer {
    id: actionProcessWatchdog
    interval: Math.max(30000, root.probeTimeoutMs * 6)
    repeat: false
    onTriggered: {
      root.actionTimedOut = true
      root.actionStatus = "No answer after "
        + Math.round(actionProcessWatchdog.interval / 1000)
        + "s — stopped. Whether it finished its work is not known; check `osm status`."
      actionProcess.signal(15)
      actionProcess.running = false
      actionStatusTimer.restart()
      root.refreshAll()
    }
  }

  KeyboardPanel {
    id: menuPanel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    // 560, measured rather than picked. A session row is no longer a name and
    // its counts: it is a name, its counts, a line saying what the session is
    // *about*, and a button that opens the conversations behind it. Laid out
    // offscreen at the old 420 with this machine's own data — a 28-character
    // session name beside `6w · 18p · 4a · <monitor>` — the name was drawn
    // 108px of the 218px it wants, cut in half on every row while the counts,
    // which read much the same from row to row, stayed whole. The identity
    // line needs the name's 218px, the counts' 303px and 8px between them:
    // 529. The conversation row's project path wants 442px and is cut at 420
    // as well. 560 clears both with a little room for a longer name or a
    // wider monitor string.
    //
    // Asking for more is safe on a small screen: `fittedContentWidth` returns
    // `min(requested, screen - margins)`, so this is a ceiling and never a
    // floor, and a narrow display gets exactly what it got before —
    // `tests/qml_row_layout.rs` goes on measuring the rows at 420, 320, 240
    // and 180 for that reason.
    contentWidth: menuPanel.fittedContentWidth(Style.space(560))
    contentHeight: menuPanel.fittedContentHeight(column.implicitHeight, Style.space(620))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      // While the filter box has focus every key belongs to it. Otherwise
      // this catcher takes keys first — it sets `Keys.priority:
      // Keys.BeforeItem` — and typing `restore` into the box would refresh
      // the status and fire a snapshot on the way past.
      blocked: filterField.activeFocus
      onCloseRequested: root.close()
      onTabRequested: function (direction) { root.switchPanel(direction) }
      onTextKey: function (t) {
        // `/`, the conventional one, and the only route from the keyboard
        // into the filter box: this panel can be summoned with a key, and
        // `KeyboardPanel` then focuses this catcher. Tab switches panel and
        // Escape closes, so before this a keyboard user could not search at
        // all — and *trying* ran the shortcuts, because `restore` starts
        // with `r` and carries an `s`.
        //
        // Only while the box is on screen. Its whole column is `visible` for
        // a *ready* engine and no other, and Qt keeps a child's `activeFocus`
        // when its parent stops being visible — so focusing it
        // unconditionally handed the keys to an item nobody can see, with
        // `blocked` above then switching this catcher off behind it. The
        // panel answered no key at all after that: not `r`, not Tab, not
        // Escape. It could only be closed with the mouse.
        if (t === "/") { if (filterField.visible) filterField.forceActiveFocus() }
        else if (t === "r" || t === "R") root.refreshAll()
        else if (t === "s" || t === "S") root.snapshotNow()
      }

      Flickable {
        id: menuFlick
        anchors.fill: parent
        contentWidth: width
        contentHeight: column.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        flickableDirection: Flickable.VerticalFlick
        interactive: contentHeight > height

        // How far one notch of the wheel moves the list: about two rows.
        readonly property int wheelStep: Style.space(60)

        // `AsNeeded` sounds like "shown when there is more below" and is not.
        // In every Qt Quick Controls style the bar is drawn at zero opacity
        // until the view is *active* — moving, hovered or pressed — so a
        // panel sitting still carries no mark at all that its list continues
        // past the bottom edge. On the maintainer's machine that list is 8
        // sessions, 20 live conversations and a count of 2448 resumable ones
        // in a card the screen keeps under ~620px: it ends mid-row, looking
        // like the end. He reported that he could not scroll.
        //
        // So the bar is shown outright whenever there is something below the
        // fold, and only then: pinned on, it would mark more content beside a
        // list that has none, which is the same class of untruth pointed the
        // other way.
        //
        // The `height > 0` guard is not decoration: during construction the
        // content is measured before the view is, so for one frame a list
        // that fits is taller than a zero-height view and the bar is switched
        // on. The style's fade-out then runs for two thirds of a second — a
        // scrollbar that appears and dies every time the panel opens.
        ScrollBar.vertical: ScrollBar {
          policy: menuFlick.height > 0 && menuFlick.contentHeight > menuFlick.height
            ? ScrollBar.AlwaysOn : ScrollBar.AsNeeded
        }

        // And the wheel is handled here rather than left to `Flickable`'s
        // own, which treats a notch as a flick: it throws the list a slightly
        // different distance each time, keeps coasting after the event, and
        // decelerates to a stop just short of the end rather than at it
        // (measured: 1699.9985 of 1700). A menu is a list to read. One notch
        // moves it one step, immediately, and stops at both ends.
        //
        // Nothing between the cursor and this handler swallows the event:
        // `qs/Ui/TextField.qml` is a QtQuick.Controls TextField and
        // `qs/Ui/Button.qml` is a rectangle with a MouseArea and a
        // HoverHandler, none of which handle wheel events. Measured in
        // tests/qml_menu_scroll.rs.
        WheelHandler {
          onWheel: function (event) {
            var notches = event.angleDelta.y / 120
            if (notches === 0) return
            var maxY = Math.max(0, menuFlick.contentHeight - menuFlick.height)
            menuFlick.contentY = Math.max(0, Math.min(maxY,
              menuFlick.contentY - notches * menuFlick.wheelStep))
          }
        }

        Column {
          id: column
          width: menuFlick.width
          spacing: Style.space(12)

          PanelHero {
            width: parent.width
            title: "Session memory"
            meta: root.status && root.status.engine_version
              ? "osm " + String(root.status.engine_version)
              : "engine not reporting a version"
            foreground: root.foreground
            fontFamily: root.fontFamily
          }

          // ---- The state line. This is the part that must never lie: one
          //      sentence naming exactly what the plugin knows.
          Text {
            width: parent.width
            wrapMode: Text.WordWrap
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
            color: root.engineState === "ready" ? root.dim : root.urgent
            text: {
              switch (root.engineState) {
              case "unknown":
                return "Checking whether the osm engine is installed…"
              case "missing":
                return "The osm engine is not installed. The marketplace copies this plugin's QML "
                  + "and nothing else — no binary, no systemd unit, no tmux hook — so nothing is "
                  + "being recorded yet."
              case "unreadable":
                return "Something ran, but it did not return status JSON. Until it does, this "
                  + "plugin has no idea what has been recorded."
              case "incompatible":
                return "This plugin speaks status protocol " + root.supportedProtocol
                  + "; the engine speaks " + root.protocolFound
                  + ". The session list is not rendered, because its shape is not one this plugin has seen. "
                  + "Update whichever of the two is older."
              case "notReady":
                return "The engine is installed and reports that it cannot run."
              default:
                return "The engine is running."
              }
            }
          }

          Text {
            visible: root.detail !== ""
            width: parent.width
            wrapMode: Text.WordWrap
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            color: root.dim
            text: root.detail
          }

          // The engine's own words, verbatim, whenever it has any.
          //
          // Shown for a *ready* engine too, not only a refusing one: `message`
          // also carries notices that do not stop the engine but do change
          // what its answers mean — a database whose incompatible schema was
          // preserved still reports ready, and its old snapshots are no longer
          // readable. Hiding that behind a healthy icon is the failure this
          // plugin is meant not to have.
          Text {
            visible: root.engineState === "notReady"
              || (root.engineState === "ready" && root.status && root.status.message)
            width: parent.width
            wrapMode: Text.WordWrap
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
            color: root.urgent
            text: root.status && root.status.message
              ? String(root.status.message)
              : "The engine reported ready:false and gave no message."
          }

          Text {
            visible: root.engineState === "missing"
            width: parent.width
            wrapMode: Text.WordWrap
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            color: root.dim
            text: "Install the engine once, then this widget fills in. The installer "
              + "verifies the binary against the SHA-256 the release workflow built it "
              + "with, which is written into the script itself:\n"
              + "  curl -fsSLO https://github.com/n8group-oss/omarchy-session-memory/releases/latest/download/install.sh\n"
              + "  sh install.sh --dry-run   # prints every step, touches nothing\n"
              + "  sh install.sh\n"
              + "Building from source is in the README, and is the only route on a "
              + "machine that is not x86_64."
          }

          Flow {
            visible: root.engineState === "missing"
            width: parent.width
            spacing: Style.space(8)

            Button {
              text: "Copy install command"
              foreground: root.foreground
              fontFamily: root.fontFamily
              bordered: true
              onClicked: root.copyInstallCommand()
            }
          }

          PanelSeparator {
            visible: root.engineState === "ready"
            foreground: root.foreground
          }

          // ---- Freshness. A snapshot that does not exist is said out loud;
          //      an empty session list under a silent header would read as
          //      "nothing to restore", which is the opposite fact.
          Column {
            visible: root.engineState === "ready"
            width: parent.width
            spacing: Style.spacing.labelGap

            PanelSectionHeader {
              text: "NEWEST SNAPSHOT"
              foreground: root.foreground
              fontFamily: root.fontFamily
            }

            Text {
              width: parent.width
              wrapMode: Text.WordWrap
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              color: root.snapshot ? root.foreground : root.urgent
              text: root.snapshot
                ? "#" + root.snapshot.id + " · " + root.ago(root.snapshot.age_secs)
                  + " · " + String(root.snapshot.state)
                  + " · " + root.countText(root.snapshot.sessions, "session", "sessions")
                : "No snapshot has been recorded. There is nothing to restore yet."
            }

            Text {
              visible: root.status && root.status.capture !== undefined
              width: parent.width
              wrapMode: Text.WordWrap
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              color: (root.status && root.captureIsAlarming(root.status.capture))
                ? root.urgent : root.dim
              text: root.status ? root.captureLine(root.status.capture) : ""
            }

            // What that snapshot knows about window placement. Its own line,
            // not appended to the capture one: the capture engine is fine and
            // saying so beside "the placement could not be read" in one
            // sentence is how a healthy engine came to look broken once
            // already.
            Text {
              visible: text !== ""
              width: parent.width
              wrapMode: Text.WordWrap
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              color: root.placementIsAlarming(root.snapshot) ? root.urgent : root.dim
              text: root.placementNote(root.snapshot)
            }
          }

          PanelSeparator {
            visible: root.engineState === "ready"
            foreground: root.foreground
          }

          // ---- The filter box. One box, above both lists, because it filters
          //      both: the maintainer asked about sessions, and with 2447
          //      conversations the conversation list is where it matters most.
          //
          //      `TextField` is the shell's own (qs.Ui), not an input built
          //      here, so it carries the same focus ring, selection colour and
          //      hover cursor as every other Omarchy panel.
          Column {
            // Named so that `tests/qml_focus.rs` can lift the box together
            // with the visibility that governs it. Extracting the field on
            // its own is what let an invisible-but-focused box through.
            id: filterBox
            visible: root.engineState === "ready"
            width: parent.width
            spacing: Style.spacing.labelGap

            PanelSectionHeader {
              text: "FILTER"
              foreground: root.foreground
              fontFamily: root.fontFamily
            }

            TextField {
              id: filterField
              width: parent.width
              placeholderText: "What it was about, session name, project, kind or id"
              foreground: root.foreground
              font.family: root.fontFamily
              onTextChanged: root.filterText = filterField.text

              // Escape clears the box; a second Escape hands the keys back to
              // the panel, which closes it. Without this the field would eat
              // the key — `keyCatcher` is blocked while this has focus, which
              // it has to be, or `r` and `s` would refresh and snapshot
              // instead of being typed — and the popup could not be closed
              // from the keyboard at all.
              Keys.onPressed: function (event) {
                if (event.key !== Qt.Key_Escape) return
                if (filterField.text !== "") filterField.text = ""
                else keyCatcher.forceActiveFocus()
                event.accepted = true
              }

              // And the box gives the keys back the moment it leaves the
              // screen. Losing readiness *while typing* is the ordinary way
              // in — the status probe reruns on a timer, and an engine
              // that stops answering takes this whole column with it —
              // and Qt leaves `activeFocus` exactly where it was. A field
              // nobody can see then held the keys while `blocked` kept the
              // catcher switched off, which is the same stranded panel by a
              // different route.
              onVisibleChanged: {
                if (!filterField.visible && filterField.activeFocus)
                  keyCatcher.forceActiveFocus()
              }
            }

            // A key nobody is told about is a key nobody has. This is the
            // whole keyboard contract of the box, in the panel that owns it.
            Text {
              width: parent.width
              wrapMode: Text.WordWrap
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              color: root.dim
              text: "Press / to type here. Escape clears it, then gives the keys back to the panel."
            }
          }

          // ---- Recorded sessions, grouped by workspace.
          Column {
            visible: root.engineState === "ready"
            width: parent.width
            spacing: Style.space(10)

            PanelSectionHeader {
              text: "RECORDED SESSIONS"
              foreground: root.foreground
              fontFamily: root.fontFamily
            }

            Text {
              visible: root.sessions.length === 0
              width: parent.width
              wrapMode: Text.WordWrap
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              color: root.dim
              text: root.snapshot
                ? "The newest snapshot recorded no sessions."
                : "Nothing recorded yet."
            }

            // How many of how many matched. Silent while the box is empty; a
            // filtered list with no such line reads as a machine that recorded
            // one session, which is a claim nothing here ever made.
            Text {
              visible: text !== ""
              width: parent.width
              wrapMode: Text.WordWrap
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              color: root.dim
              text: root.sessionsNote(root.sessions.length,
                root.matchingSessions.length,
                root.filterText)
            }

            // Two nested delegates, both with a `modelData`, so both carry
            // an id and every reference names which one it means.
            Repeater {
              model: root.groups

              Column {
                id: groupDelegate
                required property var modelData
                width: parent.width
                spacing: Style.spacing.labelGap

                PanelSectionHeader {
                  text: groupDelegate.modelData.title
                  foreground: root.foreground
                  fontFamily: root.fontFamily
                }

                Repeater {
                  model: groupDelegate.modelData.rows

                  // A session row says two things: which session it is — the
                  // name and its counts — and, under that, what it is *about*.
                  // The second line is the one the maintainer was missing:
                  // eight rows reading `4p · 4a · DP-3` are eight rows nobody
                  // can tell apart. Ask for the detail and the conversations
                  // behind that line unfold under it, each with its own title
                  // and the pane it was in.
                  //
                  // Every line is its own layout and every one follows the
                  // panel's rule: one elastic child holding unbounded data,
                  // the rest at their natural widths, a control never squeezed
                  // below its own label. Measured offscreen in
                  // `tests/qml_row_layout.rs`.
                  Column {
                    id: sessionDelegate
                    required property var modelData
                    width: parent.width
                    spacing: Style.space(2)

                    // Read once here, so nothing below has to cope with an
                    // engine that answered without them.
                    readonly property var goal: sessionDelegate.modelData.goal
                      ? sessionDelegate.modelData.goal : null
                    readonly property var conversations: sessionDelegate.modelData.conversations
                      ? sessionDelegate.modelData.conversations : []
                    // Which row is open is kept on the panel and not in the
                    // delegate: a refresh rebuilds every delegate, and a row
                    // that collapsed itself every five seconds would be a row
                    // nobody can read.
                    readonly property bool expanded: root.expandedSession
                      === String(sessionDelegate.modelData.name)

                    // One line while the name still fits on it, two when it
                    // does not: a `GridLayout` whose `columns` drops to one,
                    // which needs no reparenting and leaves every field's own
                    // rules alone.
                    //
                    // The alternative was to let the name shrink further, and
                    // that is the bug this replaces: measured offscreen at a
                    // 320px panel the name was drawn 8px wide — the ellipsis,
                    // nothing else — beside 304px of counts that read much the
                    // same on every row.
                    GridLayout {
                      id: sessionIdentity
                      width: parent.width
                      columnSpacing: Style.space(8)
                      rowSpacing: Style.space(2)
                      columns: sessionIdentity.width - sessionCounts.implicitWidth
                        - sessionIdentity.columnSpacing >= root.identityFloor ? 2 : 1

                      // The name is the elastic half: a session may be called
                      // `dev` or `omarchy-session-memory-plan4`, and nothing
                      // bounds it. It takes what the counts do not need and
                      // elides from the right, because the head of a name is
                      // what tells two sessions apart — but never below the
                      // floor, and the counts give way first.
                      Text {
                        id: sessionName
                        Layout.fillWidth: true
                        Layout.preferredWidth: 0
                        Layout.minimumWidth: Math.min(root.identityFloor, sessionIdentity.width)
                        Layout.horizontalStretchFactor: 1
                        elide: Text.ElideRight
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.bodySmall
                        color: root.foreground
                        text: String(sessionDelegate.modelData.name)
                      }

                      // The counts keep their natural width — they are the row's
                      // whole content and half of "6w · 18p · 4a · DP-3" is not
                      // a fact — but they are also what gives way: on one line
                      // they elide before the name is cut to its floor, and on
                      // two they have the whole width to themselves.
                      Text {
                        id: sessionCounts
                        Layout.fillWidth: true
                        Layout.minimumWidth: 0
                        Layout.horizontalStretchFactor: 0
                        elide: Text.ElideRight
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.caption
                        color: root.dim
                        text: sessionDelegate.modelData.windows + "w \u00b7 "
                          + sessionDelegate.modelData.panes + "p \u00b7 "
                          + sessionDelegate.modelData.agents + "a \u00b7 "
                          + root.placementText(sessionDelegate.modelData)
                      }
                    }

                    // What the session is about, and the way in to the
                    // conversations that said so.
                    GridLayout {
                      id: sessionGoalLine
                      width: parent.width
                      columnSpacing: Style.space(8)
                      rowSpacing: Style.space(2)
                      columns: sessionGoalLine.width - detailsButton.implicitWidth
                        - sessionGoalLine.columnSpacing >= root.identityFloor ? 2 : 1

                      // The goal is the elastic one and the reason this line
                      // exists, so it gets everything the button does not.
                      // ElideRight: a title reads from the front.
                      Text {
                        id: sessionGoal
                        // Transcript-derived text, drawn as text. Without this a Text is
                        // Text.AutoText: Qt decides for itself that a title like
                        // `<b>work</b>` is markup, and an `<img src=…>` one makes the
                        // panel fetch that resource when it opens. See
                        // tests/qml_title_markup.rs.
                        textFormat: Text.PlainText
                        Layout.fillWidth: true
                        Layout.preferredWidth: 0
                        Layout.minimumWidth: Math.min(root.identityFloor, sessionGoalLine.width)
                        Layout.horizontalStretchFactor: 1
                        elide: Text.ElideRight
                        font.family: root.fontFamily
                        font.pixelSize: Style.font.bodySmall
                        color: (sessionDelegate.goal && sessionDelegate.goal.title)
                          ? root.foreground : root.dim
                        text: root.goalText(sessionDelegate.modelData)
                      }

                      // Nothing to open when the session held no conversation,
                      // and a button that opens an empty list is a button that
                      // lies about there being something behind it.
                      Button {
                        id: detailsButton
                        visible: sessionDelegate.conversations.length > 0
                        Layout.minimumWidth: detailsButton.implicitWidth
                        Layout.alignment: Qt.AlignLeft | Qt.AlignVCenter
                        text: sessionDelegate.expanded ? "Hide" : "Detail"
                        foreground: root.foreground
                        fontFamily: root.fontFamily
                        bordered: true
                        onClicked: root.toggleSession(sessionDelegate.modelData.name)
                      }
                    }

                    // The detail: every conversation the session held, with
                    // its own title rather than the one the goal happened to
                    // come from, and the pane it was in.
                    Column {
                      id: sessionDetail
                      visible: sessionDelegate.expanded
                      width: parent.width
                      spacing: Style.space(2)

                      Repeater {
                        model: sessionDelegate.conversations

                        GridLayout {
                          id: conversationDetailDelegate
                          required property var modelData
                          width: parent.width
                          columnSpacing: Style.space(8)
                          rowSpacing: Style.space(2)
                          columns: conversationDetailDelegate.width
                            - conversationDetailLabel.implicitWidth
                            - conversationDetailPane.implicitWidth
                            - 2 * conversationDetailDelegate.columnSpacing
                            >= root.identityFloor ? 3 : 1

                          // Kind and a twelve-character id: bounded, so this
                          // keeps its natural width.
                          Text {
                            id: conversationDetailLabel
                            Layout.fillWidth: true
                            Layout.minimumWidth: 0
                            Layout.horizontalStretchFactor: 0
                            elide: Text.ElideRight
                            font.family: root.fontFamily
                            font.pixelSize: Style.font.caption
                            color: root.dim
                            text: String(conversationDetailDelegate.modelData.kind) + " "
                              + root.shortId(conversationDetailDelegate.modelData.native_id)
                          }

                          // The title is what the reader came here for, so it
                          // is the elastic one.
                          Text {
                            id: conversationDetailTitle
                            // Transcript-derived text, drawn as text. Without this a Text is
                            // Text.AutoText: Qt decides for itself that a title like
                            // `<b>work</b>` is markup, and an `<img src=…>` one makes the
                            // panel fetch that resource when it opens. See
                            // tests/qml_title_markup.rs.
                            textFormat: Text.PlainText
                            Layout.fillWidth: true
                            Layout.preferredWidth: 0
                            Layout.minimumWidth: Math.min(root.identityFloor,
                              conversationDetailDelegate.width)
                            Layout.horizontalStretchFactor: 1
                            elide: Text.ElideRight
                            font.family: root.fontFamily
                            font.pixelSize: Style.font.bodySmall
                            color: conversationDetailDelegate.modelData.title
                              ? root.foreground : root.dim
                            text: root.titleText(conversationDetailDelegate.modelData)
                          }

                          // Where it was, and where its title came from. Both
                          // are short and fixed, and the second is why they
                          // are together: a title osm derived from the user's
                          // first prompt is not the same claim as one the
                          // agent wrote about itself, and the difference is
                          // said here rather than left to be guessed.
                          Text {
                            id: conversationDetailPane
                            Layout.fillWidth: true
                            Layout.minimumWidth: 0
                            Layout.horizontalStretchFactor: 0
                            elide: Text.ElideRight
                            font.family: root.fontFamily
                            font.pixelSize: Style.font.caption
                            color: root.dim
                            text: root.paneText(conversationDetailDelegate.modelData)
                          }
                        }
                      }
                    }
                  }
                }
              }
            }
          }

          PanelSeparator {
            visible: root.engineState === "ready"
            foreground: root.foreground
          }

          // ---- Conversations. `live` and `resumable` are disjoint by
          //      construction in the engine, so they are shown as two facts,
          //      not one list with a flag.
          Column {
            visible: root.engineState === "ready"
            width: parent.width
            spacing: Style.spacing.labelGap

            PanelSectionHeader {
              text: "CONVERSATIONS"
              foreground: root.foreground
              fontFamily: root.fontFamily
            }

            Text {
              visible: root.agentsError !== ""
              width: parent.width
              wrapMode: Text.WordWrap
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              color: root.urgent
              text: "Could not read the conversation list: " + root.agentsError
            }

            Text {
              visible: root.agentsError === "" && root.agents === null
              width: parent.width
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              color: root.dim
              text: "Reading…"
            }

            Text {
              visible: root.agents !== null
              width: parent.width
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              color: root.foreground
              text: root.countText(root.liveAgents.length, "conversation", "conversations") + " running now"
            }

            // A non-empty `problems` means the two lists above are incomplete
            // for that agent kind — which is a different thing from that kind
            // having no conversations, and has to be said.
            Text {
              visible: root.agentProblems.length > 0
              width: parent.width
              wrapMode: Text.WordWrap
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              color: root.urgent
              text: "This list is incomplete: " + root.agentProblems.length
                + (root.agentProblems.length === 1 ? " agent could not be read." : " agents could not be read.")
            }

            // Always, not only when the list is empty: the count that exists
            // is a fact about the machine, and the drawn list is a view of
            // it that may be shorter.
            Text {
              visible: root.agents !== null
              width: parent.width
              wrapMode: Text.WordWrap
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              color: root.dim
              text: root.resumableNote(root.resumableAgents.length,
                root.matchingResumableAgents.length,
                root.shownResumableAgents.length,
                root.filterText)
            }

            Repeater {
              model: root.shownResumableAgents

              // Two lines: what the conversation is — its kind, the head of
              // its id and, the reason this row is worth reading, what it is
              // about — and under them the project it belongs to.
              //
              // The title went on the first line and the path on the second
              // because that is the order they answer questions in. With 2494
              // conversations in the store, `/home/user/projects/…` narrows
              // the list to a project and the title picks the one out of it.
              //
              // Measured offscreen at a 320px panel, the old single line gave
              // the path 49px: eleven characters of a path whose head is
              // `/home/user/projects/` on every row. Three fields would not
              // have fitted on that line either — see `tests/qml_row_layout.rs`,
              // which measures both of these.
              Column {
                id: conversationDelegate
                required property var modelData
                width: parent.width
                spacing: Style.space(2)

                GridLayout {
                  id: conversationIdentity
                  width: parent.width
                  columnSpacing: Style.space(8)
                  rowSpacing: Style.space(2)
                  columns: conversationIdentity.width - conversationLabel.implicitWidth
                    - copyResumeButton.implicitWidth - 2 * conversationIdentity.columnSpacing
                    >= root.identityFloor ? 3 : 1

                  // Kind and id are already bounded — `shortId` cuts the id to
                  // twelve characters — so this label keeps its natural width.
                  // It may still elide, but only once there is nothing else
                  // left to give.
                  Text {
                    id: conversationLabel
                    Layout.fillWidth: true
                    Layout.minimumWidth: 0
                    Layout.horizontalStretchFactor: 0
                    elide: Text.ElideRight
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.caption
                    color: root.dim
                    text: String(conversationDelegate.modelData.kind) + " "
                      + root.shortId(conversationDelegate.modelData.native_id)
                  }

                  // The title is the elastic one: it is what tells one
                  // conversation from another, and it is unbounded in the way
                  // the id and the button are not. "untitled" where the engine
                  // derived none — never a blank, which would read as a row
                  // with nothing behind it.
                  Text {
                    id: conversationTitle
                    // Transcript-derived text, drawn as text. Without this a Text is
                    // Text.AutoText: Qt decides for itself that a title like
                    // `<b>work</b>` is markup, and an `<img src=…>` one makes the
                    // panel fetch that resource when it opens. See
                    // tests/qml_title_markup.rs.
                    textFormat: Text.PlainText
                    Layout.fillWidth: true
                    Layout.preferredWidth: 0
                    Layout.minimumWidth: Math.min(root.identityFloor, conversationIdentity.width)
                    Layout.horizontalStretchFactor: 1
                    elide: Text.ElideRight
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.bodySmall
                    color: conversationDelegate.modelData.title ? root.foreground : root.dim
                    text: root.titleText(conversationDelegate.modelData)
                  }

                  // The button reserves its own width as a floor. Everything
                  // else in the row can shrink; a button that has been shrunk
                  // is a control the user cannot read and may not be able to
                  // hit, which is exactly the bug being fixed.
                  Button {
                    id: copyResumeButton
                    Layout.minimumWidth: copyResumeButton.implicitWidth
                    Layout.alignment: Qt.AlignLeft | Qt.AlignVCenter
                    text: "Copy resume"
                    foreground: root.foreground
                    fontFamily: root.fontFamily
                    bordered: true
                    onClicked: root.copyResumeCommand(conversationDelegate.modelData)
                  }
                }

                // The project, on a line of its own and therefore never
                // competing with the title for room. ElideLeft because the
                // tail of a path identifies it and the head —
                // `/home/user/projects/` on every row — does not.
                Text {
                  id: conversationPath
                  // Transcript-derived text, drawn as text. Without this a Text is
                  // Text.AutoText: Qt decides for itself that a title like
                  // `<b>work</b>` is markup, and an `<img src=…>` one makes the
                  // panel fetch that resource when it opens. See
                  // tests/qml_title_markup.rs.
                  textFormat: Text.PlainText
                  width: parent.width
                  elide: Text.ElideLeft
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.caption
                  color: root.dim
                  text: conversationDelegate.modelData.project_dir
                    ? String(conversationDelegate.modelData.project_dir)
                    : "project unknown"
                }
              }
            }
          }

          PanelSeparator { foreground: root.foreground }

          Text {
            visible: root.actionStatus !== ""
            width: parent.width
            wrapMode: Text.WordWrap
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
            color: root.restoreArmed ? root.urgent : root.dim
            text: root.actionStatus
          }

          // A Flow, not a Row: three buttons whose labels change ("Restore
          // now" becomes "Confirm restore") are not guaranteed to fit one
          // line of a 420px popup, and the third one running off the edge is
          // an action the user cannot reach. Wrapped, they are all still
          // there.
          Flow {
            id: footerActions
            width: parent.width
            spacing: Style.space(8)

            // Both engine actions are disabled, and visibly so, for every
            // state that is not a working engine. A button that looks live
            // and does nothing is the failure this whole plugin is built to
            // avoid.
            Button {
              text: "Snapshot now"
              foreground: root.foreground
              fontFamily: root.fontFamily
              bordered: true
              enabled: root.engineState === "ready" && !actionProcess.running
              opacity: enabled ? 1.0 : 0.45
              onClicked: root.snapshotNow()
            }

            Button {
              text: root.restoreArmed ? "Confirm restore" : "Restore now"
              foreground: root.restoreArmed ? root.urgent : root.foreground
              fontFamily: root.fontFamily
              bordered: true
              enabled: root.engineState === "ready" && !actionProcess.running
              opacity: enabled ? 1.0 : 0.45
              onClicked: root.restoreNow()
            }

            Button {
              text: "Refresh"
              foreground: root.foreground
              fontFamily: root.fontFamily
              bordered: true
              onClicked: root.refreshAll()
            }
          }
        }
      }
    }
  }
}
