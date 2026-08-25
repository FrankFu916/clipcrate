# clipcrate

[![CI](https://github.com/FrankFu916/clipcrate/actions/workflows/ci.yml/badge.svg)](https://github.com/FrankFu916/clipcrate/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/clipcrate.svg)](https://crates.io/crates/clipcrate)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**Terminal-first clipboard history manager.** A tiny background watcher records
everything you copy; a fuzzy-search picker pastes it back — 100% local, no
daemon you don't control, no cloud, no account.

```text
$ clipcrate pick          # fuzzy-search your clipboard history in a TUI
$ clipcrate list          # newest entries with age & size
$ clipcrate get - | pbcopy
```

## Why another clipboard manager?

Most history managers are GUI apps (Maccy, CopyQ, Ditto). Terminal people keep
switching to the mouse for one keystroke of value. `clipcrate` is built
*for* the terminal:

- **Composable** — every command reads/writes stdin/stdout pipes cleanly:
  `clipcrate get - | jq .`, `clipcrate add < file`.
- **Cross-platform by construction** — one Rust binary for macOS, Linux
  (X11 *and* Wayland) and Windows.
- **Local-first privacy** — plain JSONL on your disk, atomic rewrites,
  deny-patterns so secrets never touch storage (e.g. `^sk-`).
- **Scriptable service** — ships an installer for launchd / systemd / Windows
  Registry autostart; or run `watch` under tmux yourself.

## Install

```bash
# from crates.io (Rust 1.88+)
cargo install clipcrate

# or from source
cargo install --git https://github.com/FrankFu916/clipcrate

# or grab a prebuilt binary from Releases and put it on your PATH
```

## Quick start

```bash
clipcrate watch            # start recording (Ctrl+C stops)
clipcrate install-service  # …or let launchd/systemd/Windows do it at login

clipcrate list             # what did I copy?
clipcrate pick > file      # choose interactively, redirect anywhere
clipcrate clear --last     # oops
```

### The picker

| Key                | Action                          |
| ------------------ | ------------------------------- |
| type               | fuzzy search                    |
| ↑ / ↓, PgUp/PgDn   | move selection                  |
| Enter              | print selection to stdout       |
| Esc / Ctrl+C       | cancel                          |
| Ctrl+P             | pin/unpin (pins never expire)   |
| Delete             | remove entry                    |

`pick` prints without a trailing newline, so `cd "$(clipcrate pick)"` just
works. `pick --copy` puts the selection on the clipboard instead — pair it
with a hotkey daemon for a system-wide history:

```bash
# skhd (macOS): Cmd+Shift+V opens the picker and copies the choice
cmd + shift - v : /opt/homebrew/bin/clipcrate pick --copy

# sxhkd (Linux): super + v does the same
super + v
    clipcrate pick --copy
```

### Images

Copied screenshots are stored as PNGs (content-hashed, deduplicated) next to
the history file. `pick` copies the image back to the clipboard;
`get - > shot.png` writes bytes.

## CLI

```
clipcrate pick [--newline] [--copy]
clipcrate list [--limit N] [--json]
clipcrate get <id|- >
clipcrate copy <id>
clipcrate add [TEXT | --file F]     # or pipe stdin
clipcrate clear (<id> | --last | --all)
clipcrate pin <id>
clipcrate export [--out FILE]       # JSONL + images/
clipcrate import [--file FILE]      # dedups by content, safe to re-run
clipcrate config show | set <k> <v> | deny-add <regex>
clipcrate doctor
clipcrate watch [--poll-ms N]
clipcrate install-service [--poll-ms N] | uninstall-service | service-status
```

## Configuration

`$(clipcrate config show)` — file lives in the data dir
(`~/Library/Application Support/clipcrate` on macOS), override with
`$CLIPCRATE_HOME`.

```toml
max_entries = 1000      # unpinned cap, LRU eviction
poll_ms = 700           # watcher polling interval
min_length = 1          # ignore shorter clips
max_length = 1000000    # ignore longer clips
deny_patterns = []      # regexes that are NEVER recorded
dedup = "bump"          # bump | update | all
preview_lines = 8       # picker preview height
```

Keep secrets out of history:

```bash
clipcrate config deny-add 'sk-[A-Za-z0-9]{10,}'
clipcrate config deny-add 'ghp_[A-Za-z0-9]{20,}'
```

Config changes are hot-reloaded by a running watcher.

## Data & privacy

Everything lives under `$CLIPCRATE_HOME` (or the platform default):
`history.jsonl`, `config.toml`, `images/`. No network access, ever. Delete the
directory and there is nothing left to know about you.

## Development

```bash
cargo test        # 25 unit + 12 end-to-end CLI tests
cargo fmt && cargo clippy
```

Platform notes: X11 builds need `libxcb`; Wayland needs `wl-clipboard-rs`
runtime protocols already present in desktop sessions. macOS needs no extra
deps. Windows uses the native clipboard API.

## License

MIT — see [LICENSE](LICENSE).

---

文档（中文）：[README.zh-CN.md](README.zh-CN.md)
