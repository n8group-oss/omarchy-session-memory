# Omarchy Session Memory

Restores tmux sessions and windows after a reboot. Hyprland workspace
placement and resuming AI coding-agent conversations are planned but not
yet implemented — see "Current scope" below.

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
```

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

Plan 1 restores **tmux topology only**: sessions, windows, panes, layouts,
working directories, and the active window. It does not restore Hyprland
workspace placement or resume AI coding-agent conversations — those land in
later milestones. Restored panes come back as plain shells running in their
captured working directory; whatever process was running in a pane before
reboot (an editor, an agent, a REPL) is not relaunched.

## Design

See [docs/design.md](docs/design.md).

## License

MIT
