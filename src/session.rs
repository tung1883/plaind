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
use crate::pty::sessions;
use crate::screen::ScreenStream;

pub async fn handle(stream: TcpStream, peer: SocketAddr) {
    if let Err(e) = run(stream, peer).await {
        eprintln!("[{peer}] session ended: {e}");
    }
}

async fn run(stream: TcpStream, peer: SocketAddr) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (mut rd, mut wr) = stream.into_split();

    // Small on purpose: screen frames must not pile up here (they use try_send
    // and drop when it's full), and pty output is tiny — a deep buffer just adds
    // latency.
    let (tx, mut rx) = mpsc::channel::<Value>(16);
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

    let mut caps = vec!["pty", "session", "proc"];
    if ScreenStream::SUPPORTED {
        caps.push("screen");
    }
    if Input::SUPPORTED {
        caps.push("input");
    }
    tx.send(proto::welcome(&hostname(), os_name(), &caps)).await.ok();

    // Channel state.
    let mut attached: HashMap<i64, u64> = HashMap::new(); // channel id -> session id
    let mut screens: HashMap<i64, ScreenStream> = HashMap::new();
    let mut procs = Procs::new();
    let input = Input::new();

    while let Some(frame) = proto::read_frame(rd).await? {
        let ch = proto::get_i64(&frame, "ch").unwrap_or(-1);
        match proto::msg_type(&frame) {
            "ping" => {
                tx.send(proto::pong()).await.ok();
            }
            // Legacy path: an ephemeral session that dies when its channel closes.
            "pty.open" => {
                let cols = proto::get_i64(&frame, "cols").unwrap_or(80) as u16;
                let rows = proto::get_i64(&frame, "rows").unwrap_or(24) as u16;
                let cmd = proto::get_str(&frame, "cmd").map(String::from);
                match sessions().create(None, cols, rows, cmd.as_deref(), true) {
                    Ok(sess) => {
                        let replay = sess.attach(ch, tx.clone());
                        attached.insert(ch, sess.id);
                        if !replay.is_empty() {
                            tx.send(proto::pty_data(ch, &replay)).await.ok();
                        }
                        eprintln!("[{peer}] pty.open ch={ch} -> session {} {cols}x{rows}", sess.id);
                    }
                    Err(e) => {
                        tx.send(chan_error(ch, &format!("pty failed: {e}"))).await.ok();
                    }
                }
            }
            "session.list" => {
                tx.send(proto::session_list(ch, sessions().list())).await.ok();
            }
            // Persistent path: create a named session, or reattach to `id`.
            "session.open" => {
                let cols = proto::get_i64(&frame, "cols").unwrap_or(80) as u16;
                let rows = proto::get_i64(&frame, "rows").unwrap_or(24) as u16;
                let want = proto::get_i64(&frame, "id").map(|v| v as u64);
                let sess = match want {
                    // Reattach: if it's gone (daemon restarted, killed), say so —
                    // never silently hand back a different shell.
                    Some(id) => match sessions().get(id) {
                        Some(s) => s,
                        None => {
                            tx.send(proto::session_gone(ch, id)).await.ok();
                            continue;
                        }
                    },
                    None => {
                        let name = proto::get_str(&frame, "name").map(String::from);
                        match sessions().create(name, cols, rows, None, false) {
                            Ok(s) => s,
                            Err(e) => {
                                tx.send(chan_error(ch, &format!("shell failed: {e}"))).await.ok();
                                continue;
                            }
                        }
                    }
                };
                sess.resize(cols, rows);
                let replay = sess.attach(ch, tx.clone());
                attached.insert(ch, sess.id);
                let i = sess.info();
                tx.send(proto::session_opened(ch, i.id, &i.name, i.cols, i.rows, i.alive))
                    .await
                    .ok();
                if !replay.is_empty() {
                    tx.send(proto::pty_data(ch, &replay)).await.ok();
                }
                eprintln!("[{peer}] session.open ch={ch} -> session {} ({})", i.id, i.name);
            }
            "session.detach" => {
                if let Some(id) = attached.remove(&ch) {
                    if let Some(s) = sessions().get(id) {
                        s.detach(ch);
                    }
                }
            }
            "session.kill" => {
                let id = proto::get_i64(&frame, "id").unwrap_or(0) as u64;
                if let Some(s) = sessions().get(id) {
                    s.kill();
                }
                sessions().remove(id);
                if let Some(dead_ch) =
                    attached.iter().find(|(_, v)| **v == id).map(|(c, _)| *c)
                {
                    attached.remove(&dead_ch);
                    tx.send(proto::pty_exit(dead_ch, -1)).await.ok();
                }
            }
            "pty.data" => {
                if let (Some(id), Some(data)) =
                    (attached.get(&ch).copied(), proto::get_bin(&frame, "data"))
                {
                    if let Some(s) = sessions().get(id) {
                        s.feed(data);
                    }
                }
            }
            "pty.resize" => {
                if let Some(id) = attached.get(&ch).copied() {
                    if let Some(s) = sessions().get(id) {
                        s.resize(
                            proto::get_i64(&frame, "cols").unwrap_or(80) as u16,
                            proto::get_i64(&frame, "rows").unwrap_or(24) as u16,
                        );
                    }
                }
            }
            "pty.close" => {
                if let Some(id) = attached.remove(&ch) {
                    if let Some(s) = sessions().get(id) {
                        s.detach(ch);
                        if s.ephemeral {
                            s.kill();
                            sessions().remove(id);
                        }
                    }
                }
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
            "input.move" | "input.point" | "input.click" | "input.down" | "input.up"
            | "input.key" | "input.zoom" => {
                input.handle(&frame);
            }
            "proc.list" => {
                let (list, sys) = procs.snapshot();
                tx.send(proto::proc_list(ch, list, sys)).await.ok();
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

    // Client gone: detach every session it held. Persistent ones keep running
    // (buffering output for the next reconnect); ephemeral ones are killed.
    for (ch, id) in attached.drain() {
        if let Some(s) = sessions().get(id) {
            s.detach(ch);
            if s.ephemeral {
                s.kill();
                sessions().remove(id);
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
