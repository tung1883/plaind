//! Finding and installing Stockfish into plaind's own data folder (`<data>/engines/`).
//!
//! Windows only: the download is the official Stockfish release zip (`curl.exe`, shipped with
//! Windows 10+, fetches it; PowerShell's `Expand-Archive` unpacks it) — no HTTP/zip crates.
//! The build is picked by CPU feature: avx2, else sse41-popcnt, else plain x86-64.

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::engine::Engine;

const RELEASE_BASE: &str = "https://github.com/official-stockfish/Stockfish/releases/latest/download";

pub fn engines_dir() -> PathBuf {
    crate::paths::data_dir().join("engines")
}

/// The installed engine binary, if any. `$PLAIND_STOCKFISH` overrides (a user-supplied copy).
pub fn find() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PLAIND_STOCKFISH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let dir = engines_dir();
    let mut best: Option<PathBuf> = None;
    for e in std::fs::read_dir(&dir).ok()?.flatten() {
        let p = e.path();
        let name = p.file_name()?.to_string_lossy().to_lowercase();
        if p.is_file() && name.starts_with("stockfish") && name.ends_with(".exe") {
            best = Some(p);
        }
    }
    best
}

pub fn version(path: &Path) -> Option<String> {
    Engine::id_name(path)
}

/// Release assets to try, best first. Stockfish 19+ ships one runtime-dispatching `universal`
/// build; older releases ship one build per CPU feature level, so those follow as fallbacks.
pub fn asset_candidates() -> Vec<&'static str> {
    if cfg!(target_arch = "aarch64") {
        return vec!["stockfish-windows-arm64-universal.zip"];
    }
    let mut v = vec!["stockfish-windows-x86-64-universal.zip"];
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") {
            v.push("stockfish-windows-x86-64-avx2.zip");
        }
        if std::is_x86_feature_detected!("sse4.1") && std::is_x86_feature_detected!("popcnt") {
            v.push("stockfish-windows-x86-64-sse41-popcnt.zip");
        }
    }
    v.push("stockfish-windows-x86-64.zip");
    v
}

/// The asset `install` tries first (for status display).
pub fn asset_name() -> &'static str {
    asset_candidates()[0]
}

/// Installs Stockfish, reporting `(stage, percent)` as it goes. Returns the engine path.
/// Blocking — run on a blocking thread.
pub fn install(progress: &mut dyn FnMut(&str, u32)) -> Result<PathBuf> {
    if !cfg!(windows) {
        bail!("automatic Stockfish install is Windows-only; set PLAIND_STOCKFISH to your own binary");
    }
    let dir = engines_dir();
    std::fs::create_dir_all(&dir)?;
    progress("download", 0);
    let zip = dir.join("stockfish-download.zip");
    let mut last_err = String::new();
    let mut got = false;
    for asset in asset_candidates() {
        let url = format!("{RELEASE_BASE}/{asset}");
        let out = Command::new("curl.exe")
            .args(["-L", "--fail", "--silent", "--show-error", "--retry", "2", "-o"])
            .arg(&zip)
            .arg(&url)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| anyhow!("cannot run curl.exe: {e}"))?;
        if out.status.success() {
            got = true;
            break;
        }
        last_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let _ = std::fs::remove_file(&zip);
    }
    if !got {
        bail!("download failed: {last_err}");
    }
    let size = std::fs::metadata(&zip).map(|m| m.len()).unwrap_or(0);
    if size < 1_000_000 {
        let _ = std::fs::remove_file(&zip);
        bail!("download looks wrong ({size} bytes)");
    }
    progress("extract", 60);

    let staging = dir.join("_staging");
    let _ = std::fs::remove_dir_all(&staging);
    let ps = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!(
            "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
            ps_quote(&zip),
            ps_quote(&staging)
        ))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow!("cannot run powershell: {e}"))?;
    if !ps.status.success() {
        let _ = std::fs::remove_dir_all(&staging);
        bail!("unzip failed: {}", String::from_utf8_lossy(&ps.stderr).trim());
    }

    // The zip holds `stockfish/stockfish-windows-….exe` (+ docs). Take the exe.
    let exe = walk_for_exe(&staging).ok_or_else(|| anyhow!("no stockfish exe in the archive"))?;
    let dest = dir.join("stockfish.exe");
    let _ = std::fs::remove_file(&dest);
    std::fs::copy(&exe, &dest)?;
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&zip);
    progress("verify", 90);

    if version(&dest).is_none() {
        let _ = std::fs::remove_file(&dest);
        bail!("installed engine does not run on this CPU");
    }
    progress("done", 100);
    Ok(dest)
}

/// Escapes a path for a single-quoted PowerShell string (a user name like O'Brien would
/// otherwise end the string early).
fn ps_quote(p: &Path) -> String {
    p.display().to_string().replace('\'', "''")
}

fn walk_for_exe(dir: &Path) -> Option<PathBuf> {
    for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
        let p = e.path();
        let name = p.file_name()?.to_string_lossy().to_lowercase();
        if p.is_file() && name.starts_with("stockfish") && name.ends_with(".exe") {
            return Some(p.to_path_buf());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_quoting_doubles_single_quotes() {
        assert_eq!(ps_quote(Path::new(r"C:\Users\O'Brien\x.zip")), r"C:\Users\O''Brien\x.zip");
    }

    #[test]
    fn candidates_are_known_builds_with_universal_first() {
        let c = asset_candidates();
        assert!(c[0].contains("universal"));
        assert!(c.iter().all(|a| a.starts_with("stockfish-windows-") && a.ends_with(".zip")));
    }
}
