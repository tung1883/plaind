//! Start `plaind` automatically when the user logs in.
//!
//! It runs in the interactive user session (not a Windows service / systemd
//! system unit) — screen capture and input injection only work there.

use anyhow::{Context, Result};
use std::process::Command;

/// Register autostart if it isn't already. Called on a bare `plaind` run, so the
/// daemon is "on by default"; silent when already installed, `plaind uninstall`
/// removes it.
pub fn ensure() {
    if is_installed() {
        return;
    }
    match install() {
        Ok(()) => crate::plog!("plaind: set to start on login  (undo: plaind uninstall)"),
        Err(e) => crate::plog!("plaind: could not set autostart: {e:#}"),
    }
}

fn exe() -> Result<String> {
    Ok(std::env::current_exe()?.to_string_lossy().into_owned())
}

// --- Windows: HKCU Run key ---------------------------------------------------

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[cfg(windows)]
pub fn is_installed() -> bool {
    Command::new("reg")
        .args(["query", RUN_KEY, "/v", "plaind"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
pub fn install() -> Result<()> {
    let ok = Command::new("reg")
        .args([
            "add", RUN_KEY, "/v", "plaind", "/t", "REG_SZ", "/d",
            &format!("\"{}\"", exe()?), "/f",
        ])
        .status()
        .context("running reg add")?
        .success();
    anyhow::ensure!(ok, "reg add failed");
    Ok(())
}

#[cfg(windows)]
pub fn uninstall() -> Result<()> {
    Command::new("reg")
        .args(["delete", RUN_KEY, "/v", "plaind", "/f"])
        .status()
        .context("running reg delete")?;
    Ok(())
}

// --- macOS: LaunchAgent ------------------------------------------------------

#[cfg(target_os = "macos")]
fn plist_path() -> Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("no home dir")?
        .join("Library/LaunchAgents/com.plain.plaind.plist"))
}

#[cfg(target_os = "macos")]
pub fn is_installed() -> bool {
    plist_path().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(target_os = "macos")]
pub fn install() -> Result<()> {
    let p = plist_path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(
        &p,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.plain.plaind</string>
  <key>ProgramArguments</key><array><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict></plist>
"#,
            exe()?
        ),
    )?;
    let _ = Command::new("launchctl").args(["load", "-w"]).arg(&p).status();
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<()> {
    let p = plist_path()?;
    let _ = Command::new("launchctl").args(["unload", "-w"]).arg(&p).status();
    let _ = std::fs::remove_file(&p);
    Ok(())
}

// --- Linux / other: XDG autostart .desktop ----------------------------------

#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_path() -> Result<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .context("no config dir")?;
    Ok(base.join("autostart/plaind.desktop"))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn is_installed() -> bool {
    desktop_path().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn install() -> Result<()> {
    let p = desktop_path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(
        &p,
        format!(
            "[Desktop Entry]\nType=Application\nName=plaind\nExec={}\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
            exe()?
        ),
    )?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn uninstall() -> Result<()> {
    let _ = std::fs::remove_file(desktop_path()?);
    Ok(())
}
