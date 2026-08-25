//! End-to-end CLI tests. Every invocation runs the real binary as a child
//! process with its own `CLIPCRATE_HOME`, so tests are fully isolated from
//! each other and from the developer's actual clipboard history.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Ctx {
    home: PathBuf,
}

impl Ctx {
    fn new(tag: &str) -> Ctx {
        let home = std::env::temp_dir().join(format!(
            "clipcrate-e2e-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&home).unwrap();
        Ctx { home }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_stdin(args, None)
    }

    fn run_stdin(&self, args: &[&str], input: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clipcrate"));
        cmd.args(args).env("CLIPCRATE_HOME", &self.home);
        if let Some(input) = input {
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            let mut child = cmd.spawn().expect("spawn clipcrate");
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        } else {
            cmd.output().expect("run clipcrate")
        }
    }

    fn stdout(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "command {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn add_list_get_roundtrip() {
    let ctx = Ctx::new("roundtrip");
    ctx.run(&["add", "hello world"]);

    let list = ctx.stdout(&["list", "--limit", "10"]);
    assert!(list.contains("hello world"), "list output: {list}");

    let got = ctx.stdout(&["get", "-"]);
    assert_eq!(got, "hello world");

    // JSON mode is machine-readable.
    let json = ctx.stdout(&["list", "--json", "--limit", "1"]);
    let v: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(v["text"], "hello world");
}

#[test]
fn add_via_stdin_and_dedup_bump() {
    let ctx = Ctx::new("stdin");
    let out = ctx.run_stdin(&["add"], Some("piped content\nsecond line"));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();

    let dup = ctx.run_stdin(&["add"], Some("piped content\nsecond line"));
    assert!(dup.status.success());
    assert!(String::from_utf8_lossy(&dup.stdout).contains("already"));

    let list = ctx.stdout(&["list"]);
    assert_eq!(list.matches("piped content").count(), 1);
    assert_eq!(id, 1);
}

#[test]
fn pin_survives_clear_all() {
    let ctx = Ctx::new("pin");
    for t in ["one", "two", "three"] {
        ctx.run(&["add", t]);
    }
    // Entry ids start at 1; pin #1 ("one").
    ctx.stdout(&["pin", "1"]);
    assert_eq!(ctx.stdout(&["clear", "--all"]).trim(), "deleted 2");

    let list = ctx.stdout(&["list"]);
    assert!(list.contains("one"), "pinned must survive: {list}");
    assert!(!list.contains("two"));
    assert!(!list.contains("three"));

    // Pin toggle round-trips back to unpinned.
    assert_eq!(ctx.stdout(&["pin", "1"]).trim(), "unpinned #1");
}

#[test]
fn clear_last_and_by_id() {
    let ctx = Ctx::new("clearlast");
    for t in ["alpha", "beta"] {
        ctx.run(&["add", t]);
    }
    assert_eq!(ctx.stdout(&["clear", "--last"]).trim(), "deleted 1");
    let list = ctx.stdout(&["list"]);
    assert!(list.contains("alpha") && !list.contains("beta"));

    assert_eq!(ctx.stdout(&["clear", "1"]).trim(), "deleted 1");
    assert!(!ctx.run(&["clear", "99"]).status.success());
}

#[test]
fn export_import_between_homes() {
    let src = Ctx::new("export");
    for t in ["keep me", "me too"] {
        src.run(&["add", t]);
    }
    let dump_path = src.home.join("dump.jsonl");
    src.stdout(&["export", "--out", dump_path.to_str().unwrap()]);
    assert!(dump_path.exists());

    let dst = Ctx::new("import");
    dst.run(&["add", "pre-existing"]);
    let out = dst.stdout(&["import", "--file", dump_path.to_str().unwrap()]);
    assert!(out.contains("imported 2"), "{out}");

    let list = dst.stdout(&["list"]);
    assert!(list.contains("keep me") && list.contains("me too"));

    // Re-import skips everything.
    let again = dst.stdout(&["import", "--file", dump_path.to_str().unwrap()]);
    assert!(again.contains("skipped 2"), "{again}");
}

#[test]
fn config_set_and_deny_pattern() {
    let ctx = Ctx::new("config");
    ctx.stdout(&["config", "set", "max_entries", "5"]);
    let show = ctx.stdout(&["config", "show"]);
    assert!(show.contains("max_entries = 5"), "{show}");

    // Setting survives into the next process.
    ctx.run(&["add", "x"]);
    ctx.run(&["add", "y"]);
    let show2 = ctx.stdout(&["config", "show"]);
    assert!(show2.contains("poll_ms = 700"));

    // Deny pattern blocks matching adds with a non-zero exit.
    ctx.stdout(&["config", "deny-add", "^sk-[0-9]+$"]);
    assert!(!ctx.run(&["add", "sk-12345678"]).status.success());
    assert!(ctx.run(&["add", "normal text"]).status.success());
}

#[test]
fn invalid_regex_rejected() {
    let ctx = Ctx::new("badregex");
    assert!(!ctx
        .run(&["config", "deny-add", "(unclosed"])
        .status
        .success());
}

#[test]
fn doctor_reports_store_state() {
    let ctx = Ctx::new("doctor");
    ctx.run(&["add", "probe entry"]);
    let out = ctx.run(&["doctor"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("store"), "doctor output: {text}");
    assert!(
        text.contains("1 entries"),
        "store line should report entry count: {text}"
    );
}

#[test]
fn get_unknown_id_fails_cleanly() {
    let ctx = Ctx::new("missing");
    let out = ctx.run(&["get", "42"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no entry #42"), "{err}");
}

#[test]
fn empty_history_messages_are_helpful() {
    let ctx = Ctx::new("empty");
    let list = ctx.stdout(&["list"]);
    assert!(list.is_empty(), "fresh store lists nothing");
    let out = ctx.run(&["get", "-"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("history is empty"));
}

#[test]
fn concurrent_processes_serialize_on_lock() {
    use std::process::Command as C;
    let ctx = Ctx::new("lock");
    // Fire five adds at once; every one must succeed (flock serializes them).
    let mut children = Vec::new();
    for i in 0..5 {
        let mut c = C::new(env!("CARGO_BIN_EXE_clipcrate"));
        c.args(["add", &format!("parallel-{i}")])
            .env("CLIPCRATE_HOME", &ctx.home)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        children.push(c.spawn().expect("spawn concurrent add"));
    }
    for mut c in children {
        assert!(c.wait().unwrap().success(), "concurrent add must succeed");
    }
    let list = ctx.stdout(&["list"]);
    for i in 0..5 {
        assert!(list.contains(&format!("parallel-{i}")), "{list}");
    }
}

#[test]
fn service_status_reports_platform() {
    let ctx = Ctx::new("svc");
    let out = ctx.stdout(&["service-status"]);
    assert!(
        out.contains("platform:") && out.contains("installed:"),
        "{out}"
    );
}
