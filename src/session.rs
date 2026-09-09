//! One client connection: handshake, then a frame loop that fans messages out
//! to the pty / screen / input / proc handlers, each keyed by its channel id.

use anyhow::Result;
use rmpv::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::io::AsyncRead;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::input::Input;
use crate::pairing;
use crate::proto;
use crate::procs::Procs;
use crate::pty::PtyChannel;
use crate::screen::ScreenStream;

pub async fn handle(stream: TcpStream, peer: SocketAddr) {
    if let Err(e) = run(stream, peer).await {
        eprintln!("[{peer}] session ended: {e}");
    }
}

async fn run(stream: TcpStream, peer: SocketAddr) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (mut rd, mut wr) = stream.into_split();

    let (tx, mut rx) = mpsc::channel::<Value>(512);
    let writer = tokio::spawn(async move {
        while let Some(value) = rx.recv().await {
            if proto::write_frame(&mut wr, &value).await.is_err() {
                break;
            }
        }
    });

    let result = frame_loop(&mut rd, &tx, peer).await;
    drop(tx);
    let _ = writer.await;
    result
}

async fn frame_loop<R>(rd: &mut R, tx: &mpsc::Sender<Value>, peer: SocketAddr) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    // Handshake.
    let hello = match proto::read_frame(rd).await? {
        Some(f) => f,
        None => return Ok(()),
    };
    if proto::msg_type(&hello) != "hello" {
        tx.send(proto::error("proto", "expected hello")).await.ok();
        return Ok(());
    }
    let token = proto::get_str(&hello, "token").unwrap_or_default();
    if !pairing::is_paired(token) {
        tx.send(proto::error("auth", "unknown token — run: plaind pair"))
            .await
            .ok();
        return Ok(());
    }
    if proto::get_i64(&hello, "proto").unwrap_or(proto::PROTO) != proto::PROTO {
        tx.send(proto::error("proto", "protocol version mismatch"))
            .await
            .ok();
        return Ok(());
    }
    let device = proto::get_str(&hello, "device").unwrap_or("phone");
    eprintln!("[{peer}] paired device connected: {device}");

    let mut caps = vec!["pty", "proc"];
    if ScreenStream::SUPPORTED {
        caps.push("screen");
    }
    if Input::SUPPORTED {
        caps.push("input");
    }
    tx.send(proto::welcome(&hostname(), os_name(), &caps)).await.ok();

    // Channel state.
    let mut ptys: HashMap<i64, PtyChannel> = HashMap::new();
    let mut screens: HashMap<i64, ScreenStream> = HashMap::new();
    let mut procs = Procs::new();
    let input = Input::new();

    while let Some(frame) = proto::read_frame(rd).await? {
        let ch = proto::get_i64(&frame, "ch").unwrap_or(-1);
        match proto::msg_type(&frame) {
            "ping" => {
                tx.send(proto::pong()).await.ok();
            }
            "pty.open" => {
                let cols = proto::get_i64(&frame, "cols").unwrap_or(80) as u16;
                let rows = proto::get_i64(&frame, "rows").unwrap_or(24) as u16;
                let cmd = proto::get_str(&frame, "cmd").map(String::from);
                match PtyChannel::open(ch, cols, rows, cmd.as_deref(), tx.clone()) {
                    Ok(channel) => {
                        ptys.insert(ch, channel);
                        eprintln!("[{peer}] pty.open ch={ch} {cols}x{rows}");
                    }
                    Err(e) => {
                        tx.send(chan_error(ch, &format!("pty failed: {e}"))).await.ok();
                    }
                }
            }
            "pty.data" => {
                if let (Some(channel), Some(data)) =
                    (ptys.get(&ch), proto::get_bin(&frame, "data"))
                {
                    channel.feed(data);
                }
            }
            "pty.resize" => {
                if let Some(channel) = ptys.get(&ch) {
                    channel.resize(
                        proto::get_i64(&frame, "cols").unwrap_or(80) as u16,
                        proto::get_i64(&frame, "rows").unwrap_or(24) as u16,
                    );
                }
            }
            "pty.close" => {
                ptys.remove(&ch);
            }
            "screen.start" => {
                if !ScreenStream::SUPPORTED {
                    tx.send(chan_error(ch, "screen not supported on this host")).await.ok();
                } else {
                    let max_w = proto::get_i64(&frame, "max_w").unwrap_or(1280) as u32;
                    let fps = proto::get_i64(&frame, "fps").unwrap_or(5);
                    let cursor = proto::get(&frame, "cursor").and_then(|v| v.as_bool()).unwrap_or(true);
                    screens.insert(ch, ScreenStream::start(ch, max_w, fps, cursor, tx.clone()));
                    eprintln!("[{peer}] screen.start ch={ch} max_w={max_w} fps={fps} cursor={cursor}");
                }
            }
            "screen.stop" => {
                screens.remove(&ch);
            }
            "input.move" | "input.point" | "input.click" | "input.down" | "input.up" | "input.key" => {
                input.handle(&frame);
            }
            "proc.list" => {
                tx.send(proto::proc_list(ch, procs.snapshot())).await.ok();
            }
            "proc.kill" => {
                let pid = proto::get_i64(&frame, "pid").unwrap_or(0);
                let sig = proto::get_str(&frame, "sig").unwrap_or("TERM");
                let ok = procs.kill(pid, sig);
                eprintln!("[{peer}] proc.kill pid={pid} sig={sig} ok={ok}");
                tx.send(proto::proc_killed(ch, pid, ok)).await.ok();
            }
            other => {
                eprintln!("[{peer}] ignoring {other}");
            }
        }
    }
    Ok(())
}

fn chan_error(ch: i64, message: &str) -> Value {
    proto::map(vec![
        ("t", proto::s("error")),
        ("ch", Value::from(ch)),
        ("code", proto::s("channel")),
        ("msg", proto::s(message)),
    ])
}

fn hostname() -> String {
    #[allow(unused_mut)]
    let mut name = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default();
    if name.is_empty() {
        name = "computer".to_string();
    }
    name
}

fn os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}
