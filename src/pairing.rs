//! Pairing tokens: 32 random bytes, base64url, stored one-per-line in
//! `paired.txt` next to the config. `plaind pair` mints one and prints the
//! `plaind://` link plus a QR code for the phone.

use anyhow::{Context, Result};
use base64::Engine;
use rand::RngCore;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use crate::proto::DEFAULT_PORT;

fn paired_file() -> PathBuf {
    crate::paths::data_dir().join("paired.txt")
}

pub fn known_tokens() -> Vec<String> {
    fs::read_to_string(paired_file())
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

pub fn is_paired(token: &str) -> bool {
    known_tokens().iter().any(|t| t == token)
}

fn add_token(token: &str) -> Result<()> {
    let dir = crate::paths::data_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    if is_paired(token) {
        return Ok(());
    }
    // Append rather than rewrite — safe against a concurrent `plaind pair`.
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paired_file())?;
    writeln!(f, "{token}")?;
    Ok(())
}

/// Trust a token without minting it — used by the integration test.
pub fn trust(token: &str) -> Result<()> {
    add_token(token)
}

/// Mint and trust a fresh token, returning the `plaind://` link.
pub fn mint(port: u16) -> Result<String> {
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    add_token(&token)?;
    let ip = local_ip_address::local_ip()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    Ok(format!("plaind://{ip}:{port}/{token}"))
}

pub fn run_pair(port: u16) -> Result<()> {
    let link = mint(port)?;
    println!("\nPair this computer with the Plain phone:\n");
    print_qr(&link);
    println!("\n  {link}\n");
    println!("Paste that link into  Dev → Add computer.  The token is now trusted.");
    Ok(())
}

/// A self-contained SVG QR of `data` (1 unit per module, 4-unit quiet zone).
pub fn qr_svg(data: &str) -> String {
    use qrcode::{EcLevel, QrCode};
    let code = match QrCode::with_error_correction_level(data, EcLevel::M) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let w = code.width();
    let q = 4usize;
    let dim = w + q * 2;
    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {dim} {dim}\" \
shape-rendering=\"crispEdges\" width=\"100%\"><rect width=\"{dim}\" height=\"{dim}\" fill=\"#fff\"/>"
    );
    let m = code.to_colors();
    for y in 0..w {
        for x in 0..w {
            if m[y * w + x] == qrcode::Color::Dark {
                s.push_str(&format!(
                    "<rect x=\"{}\" y=\"{}\" width=\"1\" height=\"1\"/>",
                    x + q,
                    y + q
                ));
            }
        }
    }
    s.push_str("</svg>");
    s
}

fn print_qr(data: &str) {
    use qrcode::{EcLevel, QrCode};
    let code = match QrCode::with_error_correction_level(data, EcLevel::L) {
        Ok(c) => c,
        Err(_) => return,
    };
    let width = code.width();
    let modules = code.to_colors();
    // Two rows per printed line, using half-block characters.
    for y in (0..width).step_by(2) {
        let mut line = String::from("  ");
        for x in 0..width {
            let top = modules[y * width + x] == qrcode::Color::Dark;
            let bottom = y + 1 < width && modules[(y + 1) * width + x] == qrcode::Color::Dark;
            line.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        println!("{line}");
    }
}

pub fn default_port() -> u16 {
    DEFAULT_PORT
}
