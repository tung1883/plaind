//! Does input sent through a running plaind actually reach Windows?
//!
//! Connects like a phone, nudges the pointer 8 px right and back, and reports whether it moved.
//! Windows drops input from a normal process while an elevated window has focus, so run this
//! with the window in question focused.
//!
//!   cargo run --example inputprobe
//!
//! `PLAIND_PORT` picks the daemon (default 8471). The pairing token comes from `PLAIND_TOKEN`,
//! or else the first one in the data folder — point `PLAIND_DATA` at the daemon's `plaind-data`.

#[cfg(not(windows))]
fn main() {
    eprintln!("inputprobe is Windows-only");
}

#[cfg(windows)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use plaind::proto::{self, s};
    use rmpv::Value;
    use std::time::Duration;
    use tokio::net::TcpStream;
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    fn pointer_x() -> i32 {
        let mut p = POINT { x: 0, y: 0 };
        unsafe { GetCursorPos(&mut p) };
        p.x
    }

    let port: u16 = std::env::var("PLAIND_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(proto::DEFAULT_PORT);
    let token = std::env::var("PLAIND_TOKEN")
        .ok()
        .or_else(|| plaind::pairing::known_tokens().into_iter().next())
        .context("no pairing token: set PLAIND_TOKEN, or PLAIND_DATA to the daemon's plaind-data folder")?;

    let mut conn = TcpStream::connect(("127.0.0.1", port)).await.with_context(|| format!("no plaind on port {port}"))?;
    let hello = proto::map(vec![
        ("t", s("hello")),
        ("proto", Value::from(proto::PROTO)),
        ("token", s(&token)),
        ("device", s("inputprobe")),
    ]);
    proto::write_frame(&mut conn, &hello).await?;
    let welcome = proto::read_frame(&mut conn).await?.context("plaind closed the connection")?;
    anyhow::ensure!(proto::msg_type(&welcome) == "welcome", "plaind refused the token");

    /// Wait until the pointer has not moved for 400 ms, so a hand on the mouse is not mistaken
    /// for the nudge. Gives up after 10 s.
    async fn still_pointer() -> Option<i32> {
        let mut last = pointer_x();
        let mut quiet = 0;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let now = pointer_x();
            quiet = if now == last { quiet + 1 } else { 0 };
            last = now;
            if quiet >= 8 {
                return Some(now);
            }
        }
        None
    }

    let (mut moved, mut stayed) = (0, 0);
    for dx in [8i32, -8, 8, -8] {
        let Some(before) = still_pointer().await else {
            println!("the pointer never stopped moving: let go of the mouse and run this again");
            std::process::exit(2);
        };
        let nudge = proto::map(vec![("t", s("input.move")), ("dx", Value::from(dx as f64)), ("dy", Value::from(0.0))]);
        proto::write_frame(&mut conn, &nudge).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let delta = pointer_x() - before;
        println!("asked for {dx:+}, pointer moved {delta:+}");
        // Pointer acceleration changes the distance, never the direction.
        if delta == 0 {
            stayed += 1;
        } else if delta.signum() == dx.signum() && delta.abs() <= 40 {
            moved += 1;
        }
    }
    match (moved, stayed) {
        (4, _) => println!("MOVED: plaind's input reaches Windows"),
        (0, 4) => {
            println!("BLOCKED: plaind's input did not reach Windows");
            std::process::exit(1);
        }
        _ => {
            println!("UNCLEAR: something else was moving the pointer; run this again");
            std::process::exit(2);
        }
    }
    Ok(())
}
