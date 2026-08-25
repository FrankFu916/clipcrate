# Changelog

All notable changes to this project will be documented in this file.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.1] - 2026-08-26

### Added
- `pick --copy`: put the selection on the system clipboard instead of printing,
  enabling one-hotkey workflows via skhd/Hammerspoon/sxhkd/AutoHotkey.

### Changed
- Watcher idle cost reduced to a single pasteboard read per poll: the store is
  no longer opened every tick, and copied screenshots are no longer re-encoded
  on every poll until the clipboard changes again.

## [0.1.0] - 2026-08-25

### Added
- Background clipboard watcher (`clipcrate watch`) with polling on macOS,
  Linux (X11/Wayland) and Windows; config hot-reload.
- Full-screen fuzzy-search picker (`clipcrate pick`) with preview pane,
  pinning, deletion; prints selection pipe-friendly.
- CLI: `list` (text/JSON), `get`, `add` (arg/stdin/file), `copy`, `clear`
  (`--last`/`--all`), `pin`, `export`, `import`, `config`, `doctor`,
  `install-service`/`uninstall-service`/`service-status`.
- Append-only JSONL store with exclusive flock, atomic rewrites, LRU eviction
  that respects pins, content dedup (`bump`/`update`/`all`).
- Image support: screenshots stored as content-hashed PNGs, deduplicated,
  copied back to clipboard from the picker.
- Privacy deny-patterns (regexes never recorded), length bounds.
- Service installer for launchd (macOS), systemd user units (Linux), HKCU Run
  registry key (Windows).
- `doctor` health check.
- Test suite: 25 unit tests + 12 end-to-end CLI integration tests.
