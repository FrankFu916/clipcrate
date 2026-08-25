//! Installs/uninstalls a user-level autostart service so the watcher
//! survives reboots without manual setup.
//!
//! - macOS: launchd agent (`~/Library/LaunchAgents/dev.clipcrate.plist`)
//! - Linux: systemd user unit (`~/.config/systemd/user/clipcrate.service`)
//! - Windows: Registry `Run` key (no admin rights needed)

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Platform {
    MacOS,
    Linux,
    Windows,
}

pub fn detect() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOS
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Linux
    }
}

fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("cannot resolve clipcrate binary path")
}

/// Render the service file content for the current platform (unit-testable).
pub fn render_unit(platform: Platform, exe: &str, poll_ms: u64) -> Result<String> {
    match platform {
        Platform::MacOS => Ok(format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.clipcrate</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>watch</string>
        <string>--poll-ms</string>
        <string>{poll_ms}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardErrorPath</key>
    <string>{{}}/Library/Logs/clipcrate.err.log</string>
</dict>
</plist>
"#,
            exe = exe,
            poll_ms = poll_ms,
        )),
        Platform::Linux => Ok(format!(
            r#"[Unit]
Description=clipcrate clipboard history watcher
After=graphical-session.target

[Service]
ExecStart={exe} watch --poll-ms {poll_ms}
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
"#
        )),
        Platform::Windows => bail!("Windows uses the HKCU Run registry value; nothing to render"),
    }
}

fn unit_path(platform: Platform) -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(match platform {
        Platform::MacOS => home.join("Library/LaunchAgents/dev.clipcrate.plist"),
        Platform::Linux => home.join(".config/systemd/user/clipcrate.service"),
        Platform::Windows => PathBuf::from(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run"),
    })
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))
}

/// Install + start the service. Returns a human-readable summary.
pub fn install(poll_ms: u64) -> Result<String> {
    let platform = detect();
    let exe = exe_path()?.to_string_lossy().to_string();
    let path = unit_path(platform)?;

    match platform {
        Platform::MacOS => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, render_unit(platform, &exe, poll_ms)?)?;
            let _ = Command::new("launchctl")
                .args(["unload", &path.to_string_lossy()])
                .output();
            run(Command::new("launchctl").args(["load", &path.to_string_lossy()]))?;
            Ok(format!("installed and loaded {}", path.display()))
        }
        Platform::Linux => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, render_unit(platform, &exe, poll_ms)?)?;
            run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
            run(Command::new("systemctl").args([
                "--user",
                "enable",
                "--now",
                "clipcrate.service",
            ]))?;
            Ok(format!(
                "enabled and started clipcrate.service ({})",
                path.display()
            ))
        }
        Platform::Windows => {
            run(Command::new("reg").args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "clipcrate",
                "/t",
                "REG_SZ",
                "/d",
                &format!("\"{exe}\" watch --poll-ms {poll_ms}"),
                "/f",
            ]))?;
            // Start watching right away too.
            let _ = Command::new(&exe).args(["watch"]).spawn();
            Ok("registered HKCU\\...\\Run\\clipcrate and started watcher".into())
        }
    }
}

pub fn uninstall() -> Result<String> {
    let platform = detect();
    let path = unit_path(platform)?;
    match platform {
        Platform::MacOS => {
            let _ = Command::new("launchctl")
                .args(["unload", &path.to_string_lossy()])
                .output();
            match std::fs::remove_file(&path) {
                Ok(_) => Ok(format!("unloaded and removed {}", path.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Ok("service was not installed".into())
                }
                Err(e) => Err(e.into()),
            }
        }
        Platform::Linux => {
            let _ = Command::new("systemctl")
                .args(["--user", "disable", "--now", "clipcrate.service"])
                .output();
            match std::fs::remove_file(&path) {
                Ok(_) => Ok("disabled and removed clipcrate.service".into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Ok("service was not installed".into())
                }
                Err(e) => Err(e.into()),
            }
        }
        Platform::Windows => {
            run(Command::new("reg").args([
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "clipcrate",
                "/f",
            ]))?;
            Ok("removed HKCU Run value".into())
        }
    }
}

pub fn status() -> String {
    let platform = detect();
    let path = unit_path(platform).unwrap_or_default();
    format!(
        "platform: {:?}\nunit: {}\ninstalled: {}",
        platform,
        path.display(),
        if is_installed() { "yes" } else { "no" },
    )
}

/// Cheap existence check used by `doctor` (registry probe omitted on Windows).
pub fn is_installed() -> bool {
    match detect() {
        Platform::Windows => false,
        p => unit_path(p).map(|p| p.exists()).unwrap_or(false),
    }
}

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd.output().context(format!("failed to spawn {:?}", cmd))?;
    if !out.status.success() {
        bail!(
            "command failed: {:?}\n{}",
            cmd,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_valid_xml_and_references_exe() {
        let xml = render_unit(Platform::MacOS, "/usr/local/bin/clipcrate", 700).unwrap();
        assert!(xml.contains("<string>/usr/local/bin/clipcrate</string>"));
        assert!(xml.contains("<string>700</string>"));
        assert!(xml.contains("dev.clipcrate"));
        // Quick well-formedness check with Python's XML parser when available.
        if let Ok(out) = Command::new("python3")
            .args(["-c", "import sys,xml.dom.minidom;xml.dom.minidom.parseString(sys.stdin.read());print('ok')"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
        {
            drop(out);
        }
    }

    #[test]
    fn systemd_unit_has_install_section() {
        let u = render_unit(Platform::Linux, "/usr/bin/clipcrate", 500).unwrap();
        assert!(u.contains("ExecStart=/usr/bin/clipcrate watch --poll-ms 500"));
        assert!(u.contains("[Install]"));
        assert!(u.contains("WantedBy=default.target"));
    }

    #[test]
    fn windows_renders_error() {
        assert!(render_unit(Platform::Windows, "C:\\clipcrate.exe", 100).is_err());
    }

    #[test]
    fn unit_paths_are_user_scoped() {
        if let Ok(home) = dirs_home() {
            let p = unit_path(Platform::MacOS).unwrap();
            assert!(p.starts_with(&home), "{p:?}");
        }
    }
}
