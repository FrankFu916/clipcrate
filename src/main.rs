//! clipcrate — terminal-first clipboard history manager.
//!
//! Data layout under the data dir (`$CLIPCRATE_HOME` or platform data dir):
//!   history.jsonl     append-only event log (atomically rewritten on mutation)
//!   config.toml       user settings
//!   images/<hash>.png image payloads
//!   store.lock        advisory lock serializing all writers

mod backend;
mod config;
mod entry;
mod filter;
mod service;
mod store;
mod tui;
mod watcher;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use entry::Kind;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use store::Store;

use backend::Clipboard as _;

#[derive(Parser)]
#[command(
    name = "clipcrate",
    version,
    about = "Terminal-first clipboard history manager",
    after_help = "Run `clipcrate install-service` once so the watcher starts at login."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive fuzzy-search picker; prints the chosen clip to stdout.
    Pick {
        /// Append a trailing newline to the printed selection.
        #[arg(long)]
        newline: bool,
        /// Put the selection on the system clipboard instead of printing
        /// (images always go to the clipboard). Silent on success.
        #[arg(long)]
        copy: bool,
    },
    /// List history (newest first).
    List {
        #[arg(short, long, default_value_t = 25)]
        limit: usize,
        /// Output machine-readable JSON lines.
        #[arg(long)]
        json: bool,
    },
    /// Print one entry's payload by id (`-` means the newest).
    Get { id: String },
    /// Add text from an argument, stdin, or a file to the history.
    Add {
        /// Inline text. Omit to read stdin; use --file for files.
        text: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Copy an existing entry back to the system clipboard by id.
    Copy { id: u64 },
    /// Delete entries: by id, --last, or every unpinned entry (--all).
    Clear {
        id: Option<u64>,
        #[arg(long)]
        last: bool,
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// Toggle pin on an entry (pins survive eviction and clear --all).
    Pin { id: u64 },
    /// Export history as JSONL (plus the images/ directory) for backup/migration.
    Export {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Import previously exported JSONL; ids already present are skipped.
    Import {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Show or change settings.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Health check: clipboard access, store state, config validity, service.
    Doctor,
    /// Run the clipboard watcher in the foreground.
    Watch {
        #[arg(long)]
        poll_ms: Option<u64>,
    },
    /// Install & start a user service that runs `watch` at login.
    InstallService {
        #[arg(long, default_value_t = 700)]
        poll_ms: u64,
    },
    /// Stop and remove the user service.
    UninstallService,
    /// Show where the service unit lives and whether it is installed.
    ServiceStatus,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the active configuration.
    Show,
    /// Set `key value` (max_entries, poll_ms, min_length, max_length, dedup, preview_lines).
    Set { key: String, value: String },
    /// Add a deny regex; matching clips are never recorded.
    DenyAdd { pattern: String },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli.cmd) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn open_store() -> Result<Store> {
    // CLI commands may race the watcher (or sibling commands); wait briefly
    // for the lock instead of failing outright.
    Store::open_blocking(&Config::data_dir(), std::time::Duration::from_secs(5))
}

fn load_config() -> Result<Config> {
    Config::load(&Config::default_path())
}

fn dispatch(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Pick { newline, copy } => {
            let mut s = open_store()?;
            if s.is_empty() {
                bail!("history is empty — copy something while `clipcrate watch` runs");
            }
            if let Some(id) = tui::run_picker(&mut s)? {
                let e = s.get(id).context("entry vanished")?;
                match e.kind {
                    Kind::Image => {
                        let png = s.payload_bytes(e)?;
                        put_image_on_clipboard(&png)?;
                        println!("copied image #{id} back to the clipboard");
                    }
                    Kind::Text if copy => put_text_on_clipboard(&e.text)?,
                    Kind::Text => {
                        let mut out = std::io::stdout().lock();
                        out.write_all(e.text.as_bytes())?;
                        if newline {
                            writeln!(out)?;
                        }
                    }
                }
            }
            Ok(())
        }

        Cmd::List { limit, json } => {
            let s = open_store()?;
            let now = entry::now_ms();
            for e in s.iter_newest_first().take(limit) {
                if json {
                    println!("{}", serde_json::to_string(e)?);
                } else {
                    let pin = if e.pinned { "*" } else { " " };
                    println!(
                        "{:>6} {} {:>4} {:>9}  {}{}",
                        e.id,
                        pin,
                        e.age(now),
                        e.human_size(),
                        e.preview(70),
                        if e.is_multiline() { " ⏎" } else { "" }
                    );
                }
            }
            Ok(())
        }

        Cmd::Get { id } => {
            let s = open_store()?;
            let e = resolve_id(&s, &id)?;
            let mut out = std::io::stdout().lock();
            match e.kind {
                Kind::Image => out.write_all(&s.payload_bytes(e)?)?,
                Kind::Text => out.write_all(e.text.as_bytes())?,
            }
            Ok(())
        }

        Cmd::Add { text, file } => {
            let payload = match (text, file) {
                (Some(t), None) => t,
                (None, Some(f)) => std::fs::read_to_string(&f)
                    .with_context(|| format!("reading {}", f.display()))?,
                (None, None) => {
                    let mut buf = Vec::new();
                    std::io::stdin().read_to_end(&mut buf)?;
                    String::from_utf8(buf).context("stdin is not valid UTF-8")?
                }
                (Some(_), Some(_)) => bail!("pass either TEXT or --file, not both"),
            };
            let cfg = load_config()?;
            let f = filter::Filter::new(&cfg)?;
            if !f.accepts(&payload)? {
                bail!("content rejected by current filters (length bounds or deny pattern)");
            }
            let mut s = open_store()?;
            match s.push_text(&payload, cfg.dedup)? {
                Some(id) => println!("{id}"),
                None => println!("already in history"),
            }
            Ok(())
        }

        Cmd::Copy { id } => {
            let s = open_store()?;
            let e = s.get(id).with_context(|| format!("no entry #{id}"))?;
            match e.kind {
                Kind::Text => put_text_on_clipboard(&e.text)?,
                Kind::Image => put_image_on_clipboard(&s.payload_bytes(e)?)?,
            }
            Ok(())
        }

        Cmd::Clear { id, last, all } => {
            let mut s = open_store()?;
            let n = if all {
                s.clear_unpinned()
            } else if last {
                match s.entries.last().map(|e| e.id) {
                    Some(t) => {
                        s.delete(t);
                        1
                    }
                    None => 0,
                }
            } else if let Some(id) = id {
                if !s.delete(id) {
                    bail!("no entry #{id}");
                }
                1
            } else {
                bail!("specify an id, --last, or --all");
            };
            s.rewrite()?;
            if all && n == 0 && !s.is_empty() {
                println!("only pinned entries remain (unpin with `clipcrate pin <id>`)");
            } else {
                println!("deleted {n}");
            }
            Ok(())
        }

        Cmd::Pin { id } => {
            let mut s = open_store()?;
            match s.toggle_pin(id) {
                Some(true) => println!("pinned #{id}"),
                Some(false) => println!("unpinned #{id}"),
                None => bail!("no entry #{id}"),
            }
            s.rewrite()
        }

        Cmd::Export { out } => {
            let s = open_store()?;
            match out {
                Some(p) => {
                    let mut buf = Vec::new();
                    for e in &s.entries {
                        serde_json::to_writer(&mut buf, e)?;
                        buf.extend_from_slice(b"\n");
                    }
                    std::fs::write(&p, &buf)?;
                    println!(
                        "exported {} entries (+ images/ if present) → {}",
                        s.len(),
                        p.display()
                    );
                    let img_src = s.data_dir().join("images");
                    if img_src.exists() {
                        let dst = p
                            .parent()
                            .unwrap_or_else(|| std::path::Path::new("."))
                            .join("images");
                        let _ = copy_dir_recursive(&img_src, &dst);
                    }
                    Ok(())
                }
                None => {
                    let mut o = std::io::stdout().lock();
                    for e in &s.entries {
                        serde_json::to_writer(&mut o, e)?;
                        writeln!(o)?;
                    }
                    Ok(())
                }
            }
        }

        Cmd::Import { file } => {
            let raw = match file {
                Some(p) => std::fs::read_to_string(&p)?,
                None => {
                    let mut b = Vec::new();
                    std::io::stdin().read_to_end(&mut b)?;
                    String::from_utf8(b)?
                }
            };
            let mut s = open_store()?;
            // Import targets may be another machine's export: ids collide,
            // so dedup by content and renumber every imported entry.
            let mut next_id = s.entries.iter().map(|e| e.id).max().unwrap_or(0) + 1;
            let mut added = 0usize;
            let mut skipped = 0usize;
            for line in raw.lines().filter(|l| !l.trim().is_empty()) {
                let e: entry::Entry = serde_json::from_str(line)
                    .with_context(|| format!("bad import line: {line}"))?;
                if s.entries
                    .iter()
                    .any(|x| x.kind == e.kind && x.text == e.text)
                {
                    skipped += 1;
                    continue;
                }
                let mut e = e;
                e.pinned = false; // pins are personal to this machine
                e.id = next_id;
                next_id += 1;
                s.entries.push(e);
                added += 1;
            }
            s.entries.sort_by_key(|e| (e.ts, e.id));
            s.rewrite()?;
            println!("imported {added} new entries, skipped {skipped} already present");
            Ok(())
        }

        Cmd::Config { action } => match action {
            ConfigAction::Show => {
                let cfg = load_config()?;
                println!("{}", toml::to_string_pretty(&cfg)?);
                println!("# file: {}", Config::default_path().display());
                Ok(())
            }
            ConfigAction::Set { key, value } => {
                let mut cfg = load_config()?;
                apply_set(&mut cfg, &key, &value)?;
                cfg.save(&Config::default_path())?;
                println!("{key} = {value}");
                Ok(())
            }
            ConfigAction::DenyAdd { pattern } => {
                regex::Regex::new(&pattern).context("invalid regex")?;
                let mut cfg = load_config()?;
                if cfg.deny_patterns.contains(&pattern) {
                    println!("pattern already present");
                    return Ok(());
                }
                cfg.deny_patterns.push(pattern.clone());
                cfg.save(&Config::default_path())?;
                println!("deny pattern added: {pattern}");
                Ok(())
            }
        },

        Cmd::Doctor => doctor(),

        Cmd::Watch { poll_ms } => {
            let mut cfg = load_config()?;
            if let Some(p) = poll_ms {
                cfg.poll_ms = p.max(50);
            }
            let w =
                watcher::Watcher::new(backend::SystemClipboard::new(), Config::data_dir(), &cfg)?;
            eprintln!(
                "clipcrate watching (poll={}ms, data={}) — Ctrl+C to stop",
                cfg.poll_ms,
                Config::data_dir().display()
            );
            w.run(Arc::new(AtomicBool::new(false)))
        }

        Cmd::InstallService { poll_ms } => {
            println!("{}", service::install(poll_ms)?);
            Ok(())
        }
        Cmd::UninstallService => {
            println!("{}", service::uninstall()?);
            Ok(())
        }
        Cmd::ServiceStatus => {
            println!("{}", service::status());
            Ok(())
        }
    }
}

fn resolve_id<'a>(s: &'a Store, id: &str) -> Result<&'a entry::Entry> {
    if id == "-" {
        return s.entries.last().context("history is empty");
    }
    let n: u64 = id.parse().context("id must be a number or '-'")?;
    s.get(n).with_context(|| format!("no entry #{n}"))
}

fn put_text_on_clipboard(text: &str) -> Result<()> {
    backend::SystemClipboard::new().set_text(text)
}

fn put_image_on_clipboard(png: &[u8]) -> Result<()> {
    backend::SystemClipboard::new().set_image_png(png)
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.path().is_dir() {
            copy_dir_recursive(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

fn apply_set(cfg: &mut Config, key: &str, value: &str) -> Result<()> {
    match key {
        "max_entries" => cfg.max_entries = parse_num(value)?,
        "min_length" => cfg.min_length = parse_num(value)?,
        "max_length" => cfg.max_length = parse_num(value)?,
        "preview_lines" => cfg.preview_lines = parse_num(value)?,
        "poll_ms" => cfg.poll_ms = (parse_num(value)? as u64).max(50),
        "dedup" => {
            cfg.dedup = match value {
                "bump" => config::DedupMode::Bump,
                "update" => config::DedupMode::Update,
                "all" => config::DedupMode::All,
                other => bail!("dedup must be bump|update|all, got '{other}'"),
            };
        }
        other => bail!(
            "unknown key '{other}' (valid: max_entries, poll_ms, min_length, max_length, dedup, preview_lines)"
        ),
    }
    Ok(())
}

fn parse_num(value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .context("expected a non-negative integer")
}

fn doctor() -> Result<()> {
    let mut problems = 0usize;
    println!("clipcrate doctor");
    println!("  version  : {}", env!("CARGO_PKG_VERSION"));

    let mut cb = backend::SystemClipboard::new();
    match cb.get_text() {
        Ok(_) => println!("  clipboard: OK"),
        Err(e) => {
            problems += 1;
            println!("  clipboard: FAIL ({e})");
        }
    }

    let dir = Config::data_dir();
    match Store::open(&dir) {
        Ok(s) => println!("  store    : OK ({} entries, {})", s.len(), dir.display()),
        Err(e) => {
            problems += 1;
            println!("  store    : FAIL ({e})");
        }
    }

    match load_config() {
        Ok(cfg) => match filter::Filter::new(&cfg) {
            Ok(_) => println!("  config   : OK ({})", Config::default_path().display()),
            Err(e) => {
                problems += 1;
                println!("  config   : INVALID ({e})");
            }
        },
        Err(e) => {
            problems += 1;
            println!("  config   : UNREADABLE ({e})");
        }
    }

    println!(
        "  service  : {}",
        if service::is_installed() {
            "installed".to_string()
        } else {
            "not installed (run `clipcrate install-service`)".to_string()
        }
    );

    if problems > 0 {
        bail!("{problems} check(s) failed");
    }
    println!("all checks passed");
    Ok(())
}
