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
        #[arg(conflicts_with_all = ["last", "all"])]
        id: Option<u64>,
        #[arg(long, conflicts_with = "all")]
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
        Cmd::Pick { newline } => {
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
                    let history_path = s.data_dir().join(store::HISTORY_FILE);
                    let config_path = Config::default_path();
                    if same_existing_file(&p, &history_path)
                        || same_existing_file(&p, &config_path)
                    {
                        bail!("refusing to overwrite clipcrate's internal data file");
                    }

                    let export_root = p.parent().unwrap_or_else(|| std::path::Path::new("."));
                    // Validate and copy every referenced image before publishing
                    // the JSONL manifest, so a failed image export cannot leave
                    // behind an apparently complete backup manifest.
                    for e in s.entries.iter().filter(|e| e.kind == Kind::Image) {
                        let src = e
                            .payload_path(s.data_dir())
                            .context("unsafe image path in history")?;
                        if !src.is_file() {
                            bail!("missing image payload {}", src.display());
                        }
                        let file_name =
                            src.file_name().context("image payload has no file name")?;
                        let dst = export_root.join("images").join(file_name);
                        if let Some(parent) = dst.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        let same_file = if dst.exists() {
                            same_existing_file(&src, &dst)
                        } else {
                            false
                        };
                        if !same_file {
                            std::fs::copy(&src, &dst)
                                .with_context(|| format!("exporting image {}", src.display()))?;
                        }
                    }

                    let mut buf = Vec::new();
                    for e in &s.entries {
                        serde_json::to_writer(&mut buf, e)?;
                        buf.extend_from_slice(b"\n");
                    }
                    let tmp = p.with_extension("jsonl.export.tmp");
                    {
                        let mut out = std::fs::File::create(&tmp)?;
                        out.write_all(&buf)?;
                        out.sync_all()?;
                    }
                    if let Err(e) = replace_export_file(&tmp, &p) {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(e);
                    }
                    println!(
                        "exported {} entries (+ images/ if present) → {}",
                        s.len(),
                        p.display()
                    );
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
            let (raw, import_root) = match file {
                Some(p) => {
                    let raw = std::fs::read_to_string(&p)
                        .with_context(|| format!("reading {}", p.display()))?;
                    let root = p
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .to_path_buf();
                    (raw, Some(root))
                }
                None => {
                    let mut b = Vec::new();
                    std::io::stdin().read_to_end(&mut b)?;
                    (String::from_utf8(b)?, None)
                }
            };
            let mut s = open_store()?;
            // Import targets may be another machine's export: ids collide,
            // so dedup by content and renumber every imported entry.
            let mut next_id = s
                .entries
                .iter()
                .map(|e| e.id)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .context("history id space exhausted")?;
            let mut added = 0usize;
            let mut skipped = 0usize;
            for (line_no, line) in raw.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let e: entry::Entry = serde_json::from_str(line)
                    .with_context(|| format!("bad import line {}", line_no + 1))?;
                if s.entries
                    .iter()
                    .any(|x| x.kind == e.kind && x.text == e.text)
                {
                    skipped += 1;
                    continue;
                }
                let mut e = e;
                if e.kind == Kind::Image {
                    let src_root = import_root
                        .as_ref()
                        .context("image entries can only be imported with --file so sibling images/ payloads can be found")?;
                    let src = e
                        .payload_path(src_root)
                        .context("unsafe image path in import")?;
                    if !src.is_file() {
                        bail!("missing image payload {}", src.display());
                    }
                    let bytes = std::fs::read(&src)?;
                    image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
                        .with_context(|| format!("invalid PNG payload {}", src.display()))?;
                    let dst = e
                        .payload_path(s.data_dir())
                        .context("unsafe image path in import")?;
                    if let Some(parent) = dst.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    if dst.exists() {
                        let existing = std::fs::read(&dst)?;
                        if existing != bytes {
                            bail!("image payload collision at {}", dst.display());
                        }
                    } else {
                        let tmp = dst.with_extension("png.import.tmp");
                        {
                            let mut out = std::fs::File::create(&tmp)?;
                            out.write_all(&bytes)?;
                            out.sync_all()?;
                        }
                        std::fs::rename(&tmp, &dst)?;
                    }
                    e.size = bytes.len() as u64;
                }
                e.pinned = false; // pins are personal to this machine
                e.id = next_id;
                next_id = next_id
                    .checked_add(1)
                    .context("history id space exhausted")?;
                s.entries.push(e);
                added += 1;
            }
            s.entries.sort_by_key(|e| (e.ts, e.id));
            s.enforce_limits()?;
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
                // The store lock serializes read-modify-write config commands
                // with each other and with watcher persistence.
                let mut store = open_store()?;
                let mut cfg = load_config()?;
                apply_set(&mut cfg, &key, &value)?;
                cfg.save(&Config::default_path())?;
                if key == "max_entries" {
                    store.enforce_limits()?;
                }
                println!("{key} updated");
                Ok(())
            }
            ConfigAction::DenyAdd { pattern } => {
                regex::Regex::new(&pattern).context("invalid regex")?;
                let _store = open_store()?;
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
                watcher::Watcher::new(backend::SystemClipboard::new(), Config::data_dir(), &cfg)?
                    .with_poll_override(poll_ms);
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

fn replace_export_file(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(src, dst)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        if dst.exists() {
            std::fs::remove_file(dst)?;
        }
        std::fs::rename(src, dst)?;
        Ok(())
    }
}

fn same_existing_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
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
    match cb.probe() {
        Ok(()) => println!("  clipboard: OK"),
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
