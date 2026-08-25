# Reddit r/commandline draft

> Text post in https://www.reddit.com/r/commandline/
> Also fits r/rust ("What's everyone working on") and r/Tilde.Club-style communities.

## Title

```
clipcrate: clipboard history manager for the terminal — fuzzy-search picker,
one static Rust binary, macOS/Linux/Windows
```

## Body

```
I kept installing different clipboard managers per machine (Maccy on the Mac,
cliphist on Wayland, nothing on Windows) and wanted one thing everywhere that
stays in the terminal.

clipcrate = tiny background watcher + fuzzy-search TUI + plain CLI:

    $ clipcrate list
         3   0s      22 B  git commit -m "fix watcher race"
         2   2s     119 B  https://github.com/FrankFu916/clipcrate
         1  1m      40 B  sk-…  ← never stored if it matches a deny-pattern

- `pick` prints pipe-friendly output (`vim $(clipcrate pick)`)
- `pick --copy` + skhd/sxhkd/AutoHotkey = global hotkey history
- screenshots are captured too, content-hashed PNGs, paste them back from the picker
- history is plain JSONL in one directory — grep it, rsync it, delete it and
  there is nothing left to know about you
- one command installs launchd/systemd/Windows autostart

Everything is local; there's no account, no daemon you don't control, no cloud.

Repo + install: https://github.com/FrankFu916/clipcrate
(`cargo install clipcrate` or `brew install frankfu916/tap/clipcrate`)

What's missing before you'd daily-drive it? Currently thinking about
event-driven watch backends (lower latency than polling) and optional
encryption at rest.
```
