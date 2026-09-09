//! System-tray presence on Windows: the `pd` mark in the notification area, and
//! a right-click menu that is the daemon's whole UI when it runs windowless.

#[cfg(not(windows))]
pub fn spawn(_port: u16) {}

#[cfg(windows)]
pub fn spawn(port: u16) {
    std::thread::Builder::new()
        .name("plaind-tray".into())
        .spawn(move || run(port))
        .ok();
}

#[cfg(windows)]
fn run(port: u16) {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIconBuilder};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    let menu = Menu::new();
    let header = MenuItem::new("plaind", false, None);
    let status = MenuItem::new("starting…", false, None);
    let pair = MenuItem::new("Pairing code…", true, None);
    let open_log = MenuItem::new("Open log", true, None);
    let on_login = CheckMenuItem::new(
        "Start on login",
        true,
        crate::autostart::is_installed(),
        None,
    );
    let quit = MenuItem::new("Quit plaind", true, None);
    let sep = || PredefinedMenuItem::separator();
    let _ = menu.append_items(&[
        &header,
        &status,
        &sep(),
        &pair,
        &open_log,
        &sep(),
        &on_login,
        &sep(),
        &quit,
    ]);

    let icon = Icon::from_resource_name("APPICON", Some((32, 32))).ok();
    let mut b = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("plaind");
    if let Some(ic) = icon {
        b = b.with_icon(ic);
    }
    let _tray = match b.build() {
        Ok(t) => t,
        Err(e) => {
            crate::plog!("tray: {e}");
            return;
        }
    };

    let events = MenuEvent::receiver();
    let mut last_n = usize::MAX;

    loop {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        while let Ok(ev) = events.try_recv() {
            if ev.id == *pair.id() {
                open_pairing(port);
            } else if ev.id == *open_log.id() {
                open_path(&crate::plog::log_path());
            } else if ev.id == *on_login.id() {
                let _ = if on_login.is_checked() {
                    crate::autostart::install()
                } else {
                    crate::autostart::uninstall()
                };
            } else if ev.id == *quit.id() {
                std::process::exit(0);
            }
        }

        let n = crate::session::CONNECTED.load(Ordering::Relaxed);
        if n != last_n {
            last_n = n;
            status.set_text(if n == 0 {
                "idle — waiting".to_string()
            } else {
                format!("connected · {n} device{}", if n == 1 { "" } else { "s" })
            });
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[cfg(windows)]
fn open_path(p: &std::path::Path) {
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", ""])
        .arg(p)
        .spawn();
}

#[cfg(windows)]
fn open_pairing(port: u16) {
    let link = match crate::pairing::mint(port) {
        Ok(l) => l,
        Err(e) => {
            crate::plog!("pair: {e}");
            return;
        }
    };
    let svg = crate::pairing::qr_svg(&link);
    let dir = crate::plog::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("pairing.html");
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Pair plaind</title>\
<body style=\"background:#000;color:#fff;font-family:Consolas,ui-monospace,monospace;\
text-align:center;padding:48px 24px\">\
<div style=\"max-width:340px;margin:0 auto;background:#0b0b0b;border:1px solid #1c1c1c;\
border-radius:18px;padding:26px 22px\">\
<div style=\"font-size:20px;font-weight:500\">Pair this computer</div>\
<div style=\"width:240px;margin:22px auto;background:#fff;padding:8px;border-radius:10px\">{svg}</div>\
<div style=\"color:#8b8b8b;word-break:break-all;font-size:12px\">{link}</div>\
<div style=\"color:#484848;margin-top:14px;font-size:11px\">Dev &rarr; Add computer &rarr; scan or paste the link</div>\
</div>",
    );
    let _ = std::fs::write(&file, html);
    open_path(&file);
}
