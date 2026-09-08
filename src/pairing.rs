//! Pairing tokens: 32 random bytes, base64url, stored one-per-line in
//! `paired.txt` next to the config. `plaind pair` mints one and prints the
//! `plaind://` link plus a QR code for the phone.

use anyhow::{Context, Result};
use base64::Engine;
use rand::RngCore;
use std::fs;
use std::path::PathBuf;

use crate::proto::DEFAULT_PORT;

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("plaind")
}

fn paired_file() -> PathBuf {
    config_dir().join("paired.txt")
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
    let dir = config_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut tokens = known_tokens();
    if !tokens.iter().any(|t| t == token) {
        tokens.push(token.to_string());
    }
    fs::write(paired_file(), tokens.join("\n") + "\n")?;
    Ok(())
}

/// Trust a token without minting it — used by the integration test.
pub fn trust(token: &str) -> Result<()> {
    add_token(token)
}

pub fn run_pair(port: u16) -> Result<()> {
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    add_token(&token)?;

    let ip = local_ip_address::local_ip()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let link = format!("plaind://{ip}:{port}/{token}");

    println!("\nPair this computer with the Plain phone:\n");
    print_qr(&link);
    println!("\n  {link}\n");
    println!("Paste that link into  Dev → Add computer.  The token is now trusted.");
    Ok(())
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
