# Omarchy Session Memory

Restores tmux sessions and windows after a reboot, and puts the AI
coding-agent conversations that were running in them back into the panes
they were running in. Hyprland workspace placement is planned but not yet
implemented — see "Current scope" below.

**Status: early development.** The engine is being built first; the Omarchy
marketplace plugin follows.

## Components

- `osm` — the engine: capture, storage, restore.
- Omarchy Quattro bar widget and menu (not yet published).

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
                             # stream; a safety net alongside the hooks)
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

`store_path` is a path. No transcript *contents* are ever read or stored —
only ids, paths, sizes and modification times.

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

Enable automatic capture and restore at login:

```bash
cp systemd/osm*.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now osm-restore.service osm.service
```

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
yourself with `opencode --session <id>`. `osm status --json` says the same
thing under `agents.unsupported`:

```json
"agents": {
  "enabled": ["claude", "codex", "opencode"],
  "unsupported": [{"kind": "opencode", "reason": "OpenCode's public CLI cannot say…"}]
}
```

Leaving `opencode` in `agents.enabled` is harmless — discovery still runs for
`osm agents` — but nothing automatic will happen for it.

Hyprland workspace placement is not restored yet — that lands in a later
milestone. Any *other* process that was running in a pane before the reboot
(an editor, a REPL) is not relaunched; those panes come back as plain shells
in their captured working directory.

## Design

See [docs/design.md](docs/design.md).

## License

MIT
