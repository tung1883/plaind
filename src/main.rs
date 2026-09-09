//! `plaind` — the companion daemon for the Plain phone's Dev plugin.
//!
//!   plaind            run the daemon (default port 8471, or $PLAIND_PORT)
//!   plaind pair       mint a pairing token and print its link + QR code

use anyhow::Result;
use plaind::{pairing, session};
use tokio::net::TcpListener;

#[cfg(windows)]
fn make_dpi_aware() {
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

#[cfg(not(windows))]
fn make_dpi_aware() {}

#[tokio::main]
async fn main() -> Result<()> {
    // Screen capture, cursor read and pointer move must all share one pixel
    // space; without this they disagree under display scaling.
    make_dpi_aware();

    let args: Vec<String> = std::env::args().collect();
    let port: u16 = std::env::var("PLAIND_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(pairing::default_port);

    if args.get(1).map(String::as_str) == Some("pair") {
        return pairing::run_pair(port);
    }
    if matches!(args.get(1).map(String::as_str), Some("-h") | Some("--help") | Some("help")) {
        eprintln!("usage: plaind [pair]");
        return Ok(());
    }

    if pairing::known_tokens().is_empty() {
        eprintln!("No paired devices yet — run `plaind pair` first.");
    }

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!("plaind listening on 0.0.0.0:{port}");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        tokio::spawn(session::handle(stream, peer));
                    }
                    Err(e) => eprintln!("accept failed: {e}"),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("shutting down");
                return Ok(());
            }
        }
    }
}
