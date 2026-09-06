# Omarchy Session Memory

Restores tmux sessions and windows after a reboot, and puts the AI
coding-agent conversations that were running in them back into the panes
they were running in, and gives each restored session its terminal window
back on the workspace and monitor it was on.

**Status: early development.** The engine and the marketplace plugin are
both in the tree; neither has been released or submitted yet.

## Components

- `osm` — the engine: capture, storage, restore.
- `manifest.json`, `BarWidget.qml`, `Menu.qml` — the Omarchy Quattro bar
  widget and its menu (not yet submitted to the marketplace).

## Building

```bash
cargo build --release
install -Dm755 target/release/osm ~/.local/share/osm/bin/osm
```

## Usage

```bash
osm install-hooks           # capture on every tmux change that alters
                             # topology (splits, kills, renames, links,
                             # layout and window resizes, selection,
                             # session create/rename/close, client
                             # attach/detach)
osm uninstall-hooks        # remove the engine's tmux hooks, keep user hooks
osm snapshot                # capture now
osm snapshot --debounced    # capture now, unless one already happened
                             # within the configured debounce window
osm restore --dry-run       # show what would be restored
osm restore                 # rebuild the previous boot's sessions
osm daemon                   # fallback-interval capture loop (no event
                             # stream; a safety net alongside the hooks),
                             # and the thing that puts the hooks back on
                             # each new tmux server
osm status --json            # engine health, for the bar widget: config
                             # validity, database and tmux reachability,
                             # and capture freshness (`capture.stale`,
                             # `capture.consecutive_failures`)
osm agents --json            # every AI coding-agent conversation: the ones
                             # a pane is running now (`live`) and the ones
                             # that are only resumable (`resumable`)
osm resume <native-id>       # resume one conversation into the current
                             # pane ($TMUX_PANE)
```

### `osm agents --json`

One object with two disjoint arrays:

```json
{
  "protocol_version": 1,
  "live": [
    {
      "kind": "claude",
      "native_id": "0cfebf91-81c0-43d5-af63-c9fe7e844ede",
      "pane": "%7",
      "confidence": 0.9,
      "project_dir": "/home/u/app",
      "store_path": "/home/u/.claude/projects/-home-u-app/0cfebf91-....jsonl",
      "last_active": 1787601392,
      "size_bytes": 918273,
      "title": "Tmux memory management plugin",
      "title_source": "agent",
      "alive": true
    }
  ],
  "resumable": [ { "…": "same fields; pane and confidence are null" } ],
  "problems": []
}
```

`problems` lists any agent osm could not read, with the error. An entry there
means the two arrays above are **incomplete for that kind** — which is a
different thing from that kind having no conversations, and is why an agent
that is installed but unusable is no longer reported as an empty list.

`live` is not read from the database — it is what the running processes say
right now: a pane is running a conversation when a process in **the agent's own
lineage inside that pane** (a descendant named after the agent, or a descendant
of that) holds open a file that **is** one of the transcripts discovery found —
matched by device and inode, not by its path — *and* the pane's foreground
command is the agent. It is scored by the same confidence rule capture records
bindings with, so `osm agents` never claims a binding capture itself would
refuse.

Both qualifications are load-bearing. Any descendant would do before, so a
background job tailing another conversation's transcript could bind the pane to
*that* conversation; and any `.jsonl` whose name was a UUID counted, anywhere
on the filesystem. A symlinked or bind-mounted agent home still works, because
the file is the same file.

A conversation in `live` never also appears in `resumable`: resuming one that
is already running means attaching a second client to it, which is the case
`osm resume` refuses with `active_elsewhere`.

`store_path` is a path, and nothing in these files is read except what is
described under **What a conversation is about** below.

### What a conversation is about

`title` is one short line saying what a conversation is *for*, and
`title_source` says where that line came from. Both are `null` when osm could
derive neither, which a reader renders as *untitled* — never as a blank, and
never as an invented line.

* **`agent`** — the agent's own name for the conversation. Claude Code writes
  `ai-title` records into its transcript and revises them as the conversation
  goes on, so the last one is the current name. That is a title, not
  transcript content.

* **`first_prompt`** — **one truncated line of the user's first message**,
  from the user's opening message only: never a reply, a tool result, an
  attachment, or anything the agent itself writes as a user-shaped record
  (Claude's slash commands and local-command caveats, the plugin listing and
  `AGENTS.md` Codex injects before the person has typed anything). Whitespace
  is collapsed, control characters are dropped, and the line is cut to 120
  characters with an ellipsis.

  This one *is* transcript content, and it is a deliberate relaxation of the
  rule that osm shows titles and ids only. It is here because of what the
  alternative measures out at: of the 2497 conversations on the machine this
  was built for, 2386 are Codex's — which writes no title of its own — and 37
  of the 111 Claude ones have no `ai-title` either, so titles-only would leave
  2423 rows of 2497 blank, and a blank goal reads as a session with no
  purpose. Turn it off with `privacy.prompt_titles = false`, which leaves
  Claude conversations named by whatever their agent called them and every
  Codex conversation untitled.

Nothing is taken from a conversation osm cannot attribute: a record naming a
different conversation is refused rather than borrowed.

**Every read is bounded.** A 256 KiB suffix for Claude's title, a 128 KiB
prefix for its first prompt, a 512 KiB prefix for a Codex rollout, each sized
from measurements over a real store and stated on the constant in
`src/agent/title.rs`; a title outside those bounds is *no title*, not a reason
to read further. The scan stops at the answer and only parses a line that
could hold one. Over 2497 conversations — 14 GB of Codex rollouts, 1.4 GB of
Claude transcripts — `osm agents --json` takes the same 3.3–4.1 s it took
before titles existed, and returns 2336 of them titled.

### `osm resume <native-id>`

Resumes that conversation in the pane the command was run from, and prints
what happened:

```json
{"protocol_version":1,"kind":"claude","native_id":"0cfeb…","pane":"%7","outcome":"resumed","reason":null}
```

`outcome` is one of `resumed`, `active_elsewhere`, `pane_busy`,
`pane_missing`, `unsupported` or `failed` (with `reason` set). The exit
status is 0 only for `resumed`, so a keybinding or menu entry that does not
parse the JSON can still tell that the conversation is not in the pane.

Four preconditions must all hold before anything is sent, and the first that
does not is what gets reported:

1. `$TMUX_PANE` names a pane that is in the server's live pane list — checked
   by exact membership, never by asking tmux to resolve a target (`-t`
   matching is fuzzy: a missing pane index resolves to a *different* pane).
2. That pane's foreground command is an idle shell.
3. The pane is not in copy mode and its input is not disabled.
4. No other live process is already holding that conversation's transcript
   open — and the adapter can *say* so. An adapter that cannot tell reports
   `unsupported` and nothing is sent, because "nobody has this open" and "osm
   cannot tell whether anybody has this open" differ by exactly one live
   conversation being handed a second client.

All of this happens while `osm` holds an exclusive lock on the conversation —
keyed by `(agent kind, native id)`, under the state directory — taken *before*
the exclusivity check and released only after the identity is confirmed. Two
`osm resume` runs for the same conversation started at the same instant would
otherwise both finish their `/proc` scan before either agent opened the
transcript, both pass, and both send: the exact double attach the check exists
to prevent. The second run reports `active_elsewhere`.

A resume is confirmed by the conversation being back, never by a process with
the right name appearing: after delivery the pane is re-bound by the rules
above and must hold exactly that `(kind, id)`, and go on holding it. An agent
that starts, rejects the id and exits is reported `failed`, so the snapshot
that still knows the conversation stays retryable.

Delivery is then confirmed rather than assumed: osm waits for the pane's
foreground command to actually become the agent. A resume that silently did
nothing is reported `failed`, never `resumed` — during a boot restore that
distinction is what keeps the snapshot retryable instead of retiring the one
record of which pane held which conversation.

**Resume always uses the agent's resume form, never a create-with-id form.**
For Claude that is `claude --resume <id>`, not `claude --session-id <id>`.
`--session-id` *creates* a conversation with the given id and fails with
`Session ID is already in use` when one exists — which is precisely the case
being restored. Three of six resumes failed exactly this way during a real
recovery before this was pinned down; the adapters have no code path that can
emit the create form.

Every subcommand accepts a global `--socket <NAME>` option (or the
`OSM_TMUX_SOCKET` environment variable), passed through as `-L <NAME>` to
tmux. It targets a non-default tmux server — tests and any other tooling
that drives `osm` must always set one so they never touch the real,
default tmux server.

## What you need

**tmux 3.7 or newer, and it is not optional.** `osm` checks `tmux -V` at
startup and refuses to run on anything older, naming the version it found;
the bar widget then shows a warning icon and repeats that reason. Older tmux
silently corrupts every non-ASCII byte of the paths osm captures, so refusing
is the honest answer rather than a strictness — see
[Minimum supported tmux](#minimum-supported-tmux-37) for why no escaping on
this side can undo it.

**Hyprland 0.56 or newer, for the window half only.** 0.56 dropped the
shell-style `[workspace N silent]` rule syntax, so osm issues the Lua dispatch
form; the placement code is written and verified against 0.56.2, and no older
release has been tested against it. That is a statement about what has been
checked, not a claim that older ones fail: a dispatch the compositor refuses
is reported as a failed placement and makes the restore `partial`, so an
untested compositor produces a shortfall you can see rather than a success
that is not one.

Everything else works with no compositor at all. Say so once:

```toml
[restore]
place_windows = false
```

Captures then record tmux alone and succeed, and restores report every
session's window as `placement_disabled` — the one window outcome that counts
as finished work rather than a shortfall.

**Two installs, not one.** The Omarchy marketplace installs the QML — the bar
widget and its menu — and nothing else. `omarchy plugin add` cannot place a
binary, a systemd unit or a tmux hook, so the engine is installed separately.
Until it is, the widget says so rather than showing an empty session list.

## Installing from a release

Each `v*` tag builds `osm` for `x86_64-unknown-linux-gnu` on a GitHub-hosted
runner and publishes three files: the binary, its SHA-256, and an `install.sh`
**with that build's digest written into it**. The script downloads the binary,
verifies it against the digest it was built with, and hands the verified copy
to `osm install`; it does not fetch a checksum from the same place it fetched
the binary, which would check for corruption and for nothing else.

```bash
curl -fsSLO https://github.com/n8group-oss/omarchy-session-memory/releases/latest/download/install.sh
sh install.sh --dry-run   # prints every step, touches nothing
sh install.sh
```

Arguments are passed through to `osm install`, so `--prefix` works the same
way there. Nothing in the QML ever downloads anything: the plugin runs the
engine that is already on the machine, or says there is none.

Building from source works too, and is the only route on a non-x86_64
machine:

```bash
cargo build --release --locked
./target/release/osm install --dry-run
./target/release/osm install
```

## Installing and removing the engine

`osm install` places everything the Omarchy marketplace cannot: the binary,
both systemd user units, and the tmux hooks. `omarchy plugin add` copies QML
and nothing else, so the engine is a separate install — and, more to the
point, a separate *uninstall*: a removal that left the daemon enabled and the
hooks firing at a binary that is no longer there would be worse than none.

A tmux hook is **server state**: it lives in the tmux server process, nothing
writes it to disk, and it dies with that server. So `osm install` can only
set the hooks on the server that happens to be running at the time — and if
none is, it says so and installs everything else rather than failing half way
through. Putting them back is `osm.service`'s job: the daemon checks every
five seconds which server is on the socket, and registers the hooks on any
server that appears or replaces the one it hooked. Without that, every tmux
restart would leave the fallback interval (two minutes by default) as the
only thing capturing anything, with `osm status` reporting perfect health.

Look before you leap; both commands take `--dry-run`, which touches nothing
and prints exactly what it would do:

```bash
osm install --dry-run
osm install
```

The default prefix is `$HOME/.local`: the binary lands at `~/.local/bin/osm`
and the units at `~/.local/share/systemd/user`, which is already on systemd's
user unit search path. `--prefix <DIR>` installs somewhere else and then
leaves systemd alone, saying so, because units outside the search path cannot
be enabled by name. A real install and a real uninstall ask the running user
manager where it loads units from (`systemctl --user show -p UnitPath`) rather
than working it out from their own environment, which is a different process's
and need not agree: `XDG_DATA_HOME` set for one command changes where this
calculation thinks systemd looks and not where systemd actually looks. A dry
run never asks — it has to give the same answer on a machine with no user
manager — and so does the real run when there is definitely no manager to ask:
no `systemctl` on the machine, or nothing listening on
`$XDG_RUNTIME_DIR/systemd/private`. Both then honour `$SYSTEMD_UNIT_PATH` as
systemd documents it. The question itself is bounded, and not answering it is
not an answer: a manager that says nothing within five seconds, or refuses
while it is plainly running, leaves osm unable to say whether the units would
be loaded — so the command stops there and changes nothing. Guessing that one
wrong is invisible, which is why it is fatal: the units go to a directory
systemd never reads, the enable is skipped, and the command exits 0 while
nothing starts at the next boot. The prefix is normalised first, so `~/.local/../.local`
is the default prefix rather than a custom one that happens to write to the
same place, and the dry run and the real run make that decision once between
them — a dry run never promises systemd work the real run would decline.

Installing on top of a running engine stops `osm.service` first and starts it
again afterwards. `systemctl enable --now` only starts units that are
*inactive*, so without that the old daemon would go on running against the
new binary and the new hooks — two versions of the engine on one database. If
the running daemon will not stop, the install stops there and changes
nothing.

Whether it *is* running is asked the same way, and bounded the same way: a
manager that does not say within five seconds, or answers something this
cannot read while it is plainly there, has not said the daemon is stopped —
so the install stops before writing anything rather than treating an
unanswerable question as a "no" and replacing the binary underneath a live
daemon. A machine with no user manager at all is not uncertainty: there is
nothing for a systemd unit to be running under, and the install proceeds.
The systemd *commands* are bounded too, and they claim only what they did. A
manager that answered a question a moment ago can still wedge on the command
that follows it, which used to hang the install after its files were written
and the uninstall before it removed anything — no output, and no way out but
Ctrl-C. So `daemon-reload` has a deadline, and nothing runs after one that did not
go through: enabling units the manager has not reloaded starts them from
whatever definitions it still has cached, which is the thing the reload is
there to prevent. A command that was *killed* at its deadline is not a
command that failed — the manager may have taken it, and killing `systemctl`
does not cancel a job it queued — so the install reports what systemd did
with it, and whether the engine is running, as unknown. It never says the
engine is stopped without having asked the unit.

The start is *queued*, and says so. `enable --now osm-restore.service` starts
a `Type=oneshot` unit whose `ExecStart` is a full restore of your session
tree — systemd leaves its start timeout at `infinity`, and no deadline short
enough to be useful could tell a large session tree from a wedged one. So osm
hands the job to the manager with `--no-block` and tells you it is queued,
pointing at `systemctl --user status`; it does not wait, so it does not report
the engine as started. Killing `systemctl` would not cancel a job the manager
has already taken — which is a reason not to claim the job finished, not a
reason to hold your terminal open forever.

A stop is queued the same way and then **confirmed**, because a `stop` command
that succeeded is not a unit that stopped. The install waits up to thirty
seconds for `osm.service` to report itself inactive before replacing the
binary that daemon is running, and the uninstall waits the same way for both
units before it removes anything. Thirty seconds in total, not thirty plus a
round of questions: each `is-active` is capped by whatever is left of the
deadline, so a manager that answers slowly cannot stretch the wait by one
probe per unit. A unit nothing can speak for is not counted as stopped.

```bash
osm uninstall --dry-run
osm uninstall                     # keeps your snapshots
osm uninstall --remove-database   # and deletes them
```

Your snapshots are your own record of where you were working, so removing the
tool says nothing about whether you want to keep it: the database survives an
uninstall unless you ask for it to go. When you do ask, the removal takes the
same locks a capture and a restore take, waits up to five seconds for them,
and takes the WAL sidecars (`state.db-wal`, `state.db-shm`) with the database
— unlinking a database somebody is still writing to succeeds silently and
loses their work, and a sidecar left beside a database that no longer exists
is the next `osm`'s problem.

An uninstall that cannot finish exits non-zero and says which part is still
there. If systemd will not stop the units — or cannot be got to confirm that
they stopped — it stops before deleting anything: a unit stopped after its
binary is gone cannot run its `ExecStop`, which is the shutdown capture.

The manual equivalent, if you would rather do it yourself:

```bash
install -Dm755 target/release/osm ~/.local/bin/osm
cp systemd/osm*.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now osm-restore.service osm.service
osm install-hooks
```

## The Omarchy bar widget

`manifest.json`, `BarWidget.qml` and `Menu.qml` are the marketplace plugin: a
bar icon for the Omarchy Quattro shell and the menu behind it. They are
installed with `omarchy plugin add`, which copies QML **and nothing else** —
it cannot place a binary, a systemd unit or a tmux hook. So the plugin and
the engine are two installs, and the first thing the widget has to be able to
say is that the engine is not there.

It says it. The widget checks four things, in this order, and renders what it
found rather than an empty-but-fine state:

| what it found | bar | menu |
|---|---|---|
| engine missing (`osm` not on `PATH`) | muted broken-link icon | says the marketplace copies QML only, and gives the install command |
| protocol mismatch (`protocol_version` is not 1) | warning icon | names the version the plugin speaks and the one the engine speaks, and renders **no** session list |
| not ready (`ready: false`) | warning icon | the engine's `message`, verbatim |
| healthy | icon plus the session count | snapshot freshness, sessions grouped by workspace, resumable conversations |

Before the first probe returns, the state is *unknown* and is rendered as
unknown — not as zero sessions. Those two look identical on a bar and mean
opposite things.

`Menu.qml` is loaded by a `Loader` inside `BarWidget.qml` and is deliberately
not a second declared kind: the shell would otherwise mount it in its own
right, so the menu would exist twice and poll the engine twice.

Everything the QML runs is an argv array — `["/usr/bin/env", "osm", …]` — and
never a shell string. Session names, conversation ids and project paths all
reach these files, and the shell's own `bar.run()` and `Util.execDetached()`
helpers both end as `bash -lc <string>`, so neither is used. `/usr/bin/env`
is the launcher because its exit status distinguishes "no such program" (127)
from anything `osm` itself returns; without it, a missing binary and a
crashed one would be the same event.

Polling is every 5 seconds while the engine answers, and backs off by
doubling to a five-minute ceiling while it does not — a marketplace install
with no engine behind it is a state that does not change on its own.
`osm agents --json` is *not* on that timer: it walks `/proc` and every agent's
transcript store, so it runs when the menu opens and when you ask.

**What the menu shows of a conversation** is its kind, its id, its project
directory and one short line saying what it is about — exactly as `osm agents
--json` and `osm status --json` report them. No messages, no bodies, no
excerpts. See **What a conversation is about** above for the two sources such
a line may have, the bounds every read of one obeys, and the switch that turns
the second source off.

**A session row says what the session is about.** `osm status --json` gives
each session a `goal`: the title of the most recently active conversation in
it that has one, with the conversation's kind and id attached so the line can
never be read against the wrong one. Not a summary of several — osm does not
write sentences nobody said — and not the biggest or busiest conversation's,
because after a reboot what you want back is what you were doing last. Every
conversation the session held is listed beside it in `conversations`, newest
first, each with its own title and the window and pane it was in; the menu
draws that list under the row when you ask for the detail. A session whose
conversations osm could name nothing from reads *untitled*, and one that held
no conversation says so.

**The search box matches titles too**, first: with 2497 conversations a
project path narrows the list to a project, and after that what tells them
apart is what each was for. It matches a session's name, its goal and the
titles behind it, and a conversation's title, project, kind and id — every
field that is on screen, and nothing that is not.

**There is no focus or kill button, on purpose.** What the menu lists is the
newest *snapshot* — what osm would restore — not the live tmux server, and a
session in that list may have been closed since. A button that killed "the
session called dev" would be acting on a name, against a server this plugin
never looked at, with no way to confirm it hit the thing on screen. The
engine has no verified command for either action, and inventing one in QML
would put the decision in the layer least able to check it. `osm resume` is
listed the same way and for the same reason: it resumes into the pane it is
run from (`$TMUX_PANE`), and a bar popup is not in a pane, so the menu copies
the command for you to run there instead of running it somewhere it would
always fail.

**The QML is not covered the way the engine is.** `tests/manifest.rs` checks
the manifest against what Omarchy's `PluginRegistry` actually requires, that
every entry point names a file that exists, and that neither QML file ever
hands a string to a shell. Whether the widget *renders* correctly is not
checkable without a running Quattro shell, and this repository does not have
one in CI. The mitigation is structural rather than aspirational: every state
the widget renders comes from JSON the engine's own tests cover, so a widget
bug is a display bug and not a data-loss one. It is still a real gap.

## Running the tests

```bash
cargo test --all --no-default-features -- --test-threads=1
```

`--no-default-features` compiles out `Tmux::default_server()`, the only way
to build a `Tmux` with no `-L` socket, so no test binary can address the
developer's real tmux server even by accident. CI runs the suite this way
against tmux 3.7c, and separately asserts that osm **refuses to run** on
Debian bookworm's tmux 3.3a rather than misbehaving on it.

Minimum supported Rust version: **1.85** (the committed `Cargo.lock`
contains a dependency using edition 2024).

## Minimum supported tmux: 3.7

`osm` checks `tmux -V` at startup and exits with an error naming the version
it found if that version is older than 3.7. This will exclude you if you are
on a stock distribution:

| distribution | tmux | works with osm |
|---|---|---|
| Debian bookworm | 3.3a | no |
| Ubuntu 24.04 | 3.4 | no |
| Debian trixie | 3.5a | no |
| Arch, Fedora 41+, Homebrew | 3.7 or newer | yes |

The reason is not a nicety. tmux ≤ 3.6 rewrites every byte of `-F` format
output that it does not consider printable ASCII — newlines, tabs, and **every
byte of every non-ASCII character** — to `_`, inside the server, before osm
receives the value. A pane sitting in `/home/u/żółć` is captured as
`/home/u/______` and restored into *that* path. The corruption happens
upstream of osm, so no escaping on this side can undo it, and the failure is
silent: the snapshot looks fine and the directory is simply gone from the only
record of where the pane was.

osm passes `-u` on every tmux invocation, which is the other half of the same
problem: tmux applies the same sanitisation for any client whose environment
does not name a UTF-8 locale, and a systemd user unit or a tmux hook often has
no locale at all. You do not need to configure anything for that.

Refusing to start is the honest answer for the version. If your distribution
ships an older tmux, build a current one — the release tarballs at
<https://github.com/tmux/tmux/releases> need only `libevent` and `ncurses`:

```bash
curl -fsSLO https://github.com/tmux/tmux/releases/download/3.7c/tmux-3.7c.tar.gz
tar -xzf tmux-3.7c.tar.gz && cd tmux-3.7c
./configure --prefix=/usr/local && make && sudo make install
```

If the state database was written by a different build of `osm`, it is never
dropped: where the shapes allow it, it is migrated in place, and otherwise it
is moved aside as `state.db.v<N>.bak` and a fresh one takes its name.
`osm status --json` reports that under `database.preserved`.

## The `@osm-server-id` server option

`osm` keeps each running tmux server's identity in a server-scope user option
called `@osm-server-id`, minted from `/dev/urandom` the first time it looks at
a server and gone the moment that server exits. It is what tells one tmux
server from the next one on the same socket: tmux hands out `$0`, `@0` and
`%0` again to every server it starts, so without it a restore's record of
which window it built could be believed by a capture taken after a tmux
restart, and a session could be linked into an unrelated window that merely
reuses the id.

**Do not set it yourself, and do not restore a dump of server options that
contains it.** A value `osm` did not mint is not evidence of anything — the
same line in a `.tmux.conf` applies to every server you ever start — so `osm`
refuses to capture or restore against a server whose `@osm-server-id` is not
in the form it writes, and says so. Options of your own under any other name
are not `osm`'s business and are neither read nor written.

## Current scope

**tmux topology**: sessions, windows, panes, layouts, working directories,
and the active window.

**AI coding-agent conversations**: a pane running **Claude Code** or **Codex**
has its conversation recorded during capture and resumed after a reboot,
subject to the preconditions above and to `agents.auto_resume` /
`agents.auto_resume_max_age_mins` (default: resume automatically when the
conversation was active within the last 30 minutes; older ones are left as
shells for you to pick up with `osm resume`). A pane whose conversation did
not come back makes the restore `partial`, so its snapshot stays retryable.
Each conversation also carries one short line saying what it is about, and
each session the goal of its most recent titled conversation — see **What a
conversation is about** above.

**OpenCode: automatic capture and resume are not supported.** Everything osm
does by itself rests on two questions it must be able to answer from the
machine — *which conversation is this pane running*, and *does anything else
have this conversation open* — and OpenCode's public CLI answers neither.
This adapter is deliberately confined to that CLI, because OpenCode's on-disk
store has changed shape across versions and is not a stable interface.

So osm does not bind a pane to an OpenCode conversation, does not resume one
during a restore, and refuses `osm resume <opencode-id>` with `unsupported`
rather than risk attaching a second client to a conversation already running
somewhere else. `osm agents --json` still lists them, and you can open one
yourself with `opencode --session <id>`. osm derives no title for an OpenCode
conversation either, for the same reason: it never reads that store, so it has
nothing to say about what one is for, and *untitled* is the honest answer.
`osm status --json` says the same thing under `agents.unsupported`:

```json
"agents": {
  "enabled": ["claude", "codex", "opencode"],
  "unsupported": [{"kind": "opencode", "reason": "OpenCode's public CLI cannot say…"}]
}
```

Leaving `opencode` in `agents.enabled` is harmless — discovery still runs for
`osm agents` — but nothing automatic will happen for it.

**Hyprland window placement**: every capture also records which terminal
window each session was attached to, and which workspace and monitor that
window was on, and a restore opens a terminal per delivered session and puts
it back there. A session whose window did not come back makes the restore
`partial`, so its snapshot stays retryable.

Because that placement is part of the state osm promises to keep, a capture
that is asked for it and **cannot read it** — Hyprland not answering,
`hyprctl` printing something that is not a client list, the tmux server
changing identity mid-read — fails and is retried, instead of recording "no
session has a terminal window" over the last good layout and pruning that
layout out of retention.

On a machine with no compositor, say so once:

```toml
[restore]
place_windows = false
```

Captures then record tmux alone and succeed, and restores report every
session's window as `placement_disabled` — the one window outcome that counts
as finished work rather than a shortfall. `restore.terminal` chooses which
terminal a restore opens (`auto`, `ghostty`, `alacritty`, `kitty`, `foot`);
`auto` prefers the terminal the session was captured in and falls back to the
first one installed.

Any *other* process that was running in a pane before the reboot (an editor, a
REPL) is not relaunched; those panes come back as plain shells in their
captured working directory.

## Design

See [docs/design.md](docs/design.md).

## License

MIT
