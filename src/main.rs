//! `plaind` — the companion daemon for the Plain phone's Dev plugin.
//!
//!   plaind            run the daemon (starts on login, tray icon, no window)
//!   plaind pair       mint a pairing token and print its link + QR code
//!   plaind uninstall  stop starting on login
//
// Release builds are windowless (no console); `AttachConsole` below reclaims the
// parent terminal for the text subcommands.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::Result;
use plaind::{autostart, pairing, paths, plog, session, tray};
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

#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_console() {}

#[tokio::main]
async fn main() -> Result<()> {
    // Screen capture, cursor read and pointer move must all share one pixel
    // space; without this they disagree under display scaling.
    make_dpi_aware();

    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str);
    let port: u16 = std::env::var("PLAIND_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(pairing::default_port);

    match cmd {
        Some("pair") => {
            attach_console();
            return pairing::run_pair(port);
        }
        Some("install") | Some("autostart") => {
            attach_console();
            autostart::install()?;
            println!("plaind will start on login.");
            return Ok(());
        }
        Some("uninstall") => {
            attach_console();
            autostart::uninstall()?;
            println!("plaind will no longer start on login.");
            return Ok(());
        }
        Some("-h") | Some("--help") | Some("help") => {
            attach_console();
            println!("usage: plaind [pair | install | uninstall]");
            return Ok(());
        }
        _ => {}
    }

    // Daemon run.
    attach_console(); // harmless when launched from a terminal; no-op on login
    plog::init();
    plog!("plaind starting on :{port}  ({})", if paths::is_portable() { "portable" } else { "installed" });

    // An installed copy registers login-autostart by default; a portable copy
    // stays out of the registry unless asked (tray toggle / `plaind install`).
    if !paths::is_portable() && std::env::var_os("PLAIND_NO_AUTOSTART").is_none() {
        autostart::ensure();
    }
    tray::spawn(port);

    if pairing::known_tokens().is_empty() {
        plog!("no paired devices yet — use the tray's \"Pairing code…\", or run `plaind pair`");
    }

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    plog!("listening on 0.0.0.0:{port}");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => { tokio::spawn(session::handle(stream, peer)); }
                    Err(e) => plog!("accept failed: {e}"),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                plog!("shutting down");
                return Ok(());
            }
        }
    }
}
