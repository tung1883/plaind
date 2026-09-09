//! Where plaind keeps its state.
//!
//! **Portable by default.** State lives in a `plaind-data` folder next to the
//! executable, so the whole thing runs from a USB stick or any directory. If
//! the exe sits somewhere unwritable (Program Files, a read-only mount) it
//! falls back to the OS config dir. `$PLAIND_DATA` overrides both.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The state directory, created if missing.
pub fn data_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = resolve();
        let _ = std::fs::create_dir_all(&d);
        d
    })
    .clone()
}

/// True when state lives next to the exe (a portable copy). An installed copy
/// (state in the OS config dir) auto-registers login-autostart; a portable one
/// leaves the registry alone unless asked.
pub fn is_portable() -> bool {
    static P: OnceLock<bool> = OnceLock::new();
    *P.get_or_init(|| std::env::var_os("PLAIND_DATA").is_none() && portable_dir().is_some())
}

fn resolve() -> PathBuf {
    if let Some(d) = std::env::var_os("PLAIND_DATA") {
        return PathBuf::from(d);
    }
    if let Some(d) = portable_dir() {
        return d;
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("plaind")
}

fn portable_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join("plaind-data");
    std::fs::create_dir_all(&dir).ok()?;
    let probe = dir.join(".write-test");
    std::fs::write(&probe, b"").ok()?;
    let _ = std::fs::remove_file(&probe);
    Some(dir)
}
