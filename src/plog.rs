//! Tiny logger. When plaind runs windowless there's no console, so lines go to
//! `<data-local>/plaind/plaind.log` (and to stderr too when a console exists).

use std::io::Write;
use std::sync::{Mutex, OnceLock};

static FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

pub fn log_dir() -> std::path::PathBuf {
    crate::paths::data_dir()
}

pub fn log_path() -> std::path::PathBuf {
    log_dir().join("plaind.log")
}

pub fn init() {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = log_path();
    // rotate once past ~2 MB
    if std::fs::metadata(&path).map(|m| m.len() > 2_000_000).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join("plaind.log.1"));
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = FILE.set(Mutex::new(f));
    }
}

pub fn line(args: std::fmt::Arguments) {
    if let Some(m) = FILE.get() {
        if let Ok(mut f) = m.lock() {
            let _ = writeln!(f, "{args}");
        }
    }
    // Also echo to stderr when it's real (dev builds / launched from a terminal).
    if has_console() {
        eprintln!("{args}");
    }
}

#[cfg(windows)]
fn has_console() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    unsafe { !GetConsoleWindow().is_null() }
}

#[cfg(not(windows))]
fn has_console() -> bool {
    true
}

#[macro_export]
macro_rules! plog {
    ($($t:tt)*) => { $crate::plog::line(format_args!($($t)*)) };
}
