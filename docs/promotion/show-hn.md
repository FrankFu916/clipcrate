# Show HN draft

> Post when there is a demo GIF in the README. Title exactly as below.
> Submit at https://news.ycombinator.com/submit (no promotional tone in title).

## Title

```
Show HN: Clipcrate – clipboard history for the terminal, one static binary
```

## First comment (post immediately after submitting)

```
Hi HN! I built clipcrate because every clipboard-history manager I liked was a
GUI app (Maccy, CopyQ, Ditto), and the terminal-native ones each supported only
one platform — clipcat needs X11, cliphist needs Wayland. I wanted one tool I
could put on every machine I use.

clipcrate is a single static Rust binary (~1.4 MB) that works on macOS, Linux
(X11 and Wayland) and Windows:

- `clipcrate watch` records everything you copy (text + screenshots)
- `clipcrate pick` is a fuzzy-search TUI; the selection prints to stdout with
  no trailing newline, so `cd "$(clipcrate pick)"` works
- `clipcrate pick --copy` pairs with skhd/sxhkd/AutoHotkey for a system-wide
  hotkey
- storage is append-only JSONL next to your dotfiles, atomic rewrites, LRU
  eviction, pins survive eviction
- deny-patterns: `clipcrate config deny-add 'ghp_[A-Za-z0-9]{20,}'` means
  tokens never touch disk even if you accidentally copy them
- `install-service` wires up launchd / systemd / Windows autostart in one go

Design choices worth discussing:

1. Polling instead of event hooks. One code path for four platforms, no extra
   permissions, ~zero idle cost after gating on clipboard changes. Event-driven
   backends (XFixes, Wayland data-control) are on the roadmap as an optional
   latency win.
2. Local-only, no sync. Sync is the obvious feature request and the obvious
   privacy foot-gun; the current answer is "export/import JSONL through your
   own Syncthing/git".
3. The store format is plain JSONL so you can always leave.

Install: `cargo install clipcrate`, `brew install frankfu916/tap/clipcrate`,
or grab a binary from Releases.

Repo: https://github.com/FrankFu916/clipcrate

Feedback welcome — especially on what would make this useful enough to replace
your current clipboard manager rather than sit next to it.
```
