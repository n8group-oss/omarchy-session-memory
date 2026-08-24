# Omarchy Session Memory

Restores tmux sessions, Hyprland workspace placement, and AI coding-agent
conversations after a reboot.

**Status: early development.** The engine is being built first; the Omarchy
marketplace plugin follows.

## Components

- `osm` — the engine: capture, storage, restore.
- Omarchy Quattro bar widget and menu (not yet published).

## Building

```bash
cargo build --release
```

## Design

See [docs/design.md](docs/design.md).

## License

MIT
