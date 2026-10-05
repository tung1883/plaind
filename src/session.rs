//! One client connection: handshake, then a frame loop that fans messages out
//! to the pty / screen / input / proc handlers, each keyed by its channel id.

use anyhow::Result;
use rmpv::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncRead;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::clip;
use crate::filesync::{self, PutState};
use crate::input::Input;
use crate::metrics::Metrics;
use crate::pairing;
use crate::proto;
use crate::procs::Procs;
use crate::pty::sessions;
use crate::screen::ScreenStream;

/// How many phones are connected right now (for the tray label).
pub static CONNECTED: AtomicUsize = AtomicUsize::new(0);

pub async fn handle(stream: TcpStream, peer: SocketAddr) {
    CONNECTED.fetch_add(1, Ordering::Relaxed);
    if let Err(e) = run(stream, peer).await {
        crate::plog!("[{peer}] session ended: {e}");
    }
    CONNECTED.fetch_sub(1, Ordering::Relaxed);
}

async fn run(stream: TcpStream, peer: SocketAddr) -> Result<()> {
    stream.set_nodelay(true).ok();
    let (mut rd, mut wr) = stream.into_split();

    // Two queues into one writer. `tx` carries everything interactive (shell output, acks,
    // pongs, replies); `tx_bulk` carries screen frames and file downloads. The writer always
    // takes from `tx` first, so a typed key's echo only ever waits behind the one frame that is
    // already being written, not behind a backlog of screen frames. Both are small on purpose:
    // screen frames must not pile up (they use try_send and drop when full) and a deep buffer
    // just adds latency.
    let (tx, mut rx) = mpsc::channel::<Value>(16);
    let (tx_bulk, mut rx_bulk) = mpsc::channel::<Value>(16);
    let writer = tokio::spawn(async move {
        let (mut hi_open, mut lo_open) = (true, true);
        loop {
            let value = tokio::select! {
                biased;
                v = rx.recv(), if hi_open => match v {
                    Some(v) => v,
                    None => { hi_open = false; continue; }
                },
                v = rx_bulk.recv(), if lo_open => match v {
                    Some(v) => v,
                    None => { lo_open = false; continue; }
                },
                else => break,
            };
            // frames still waiting behind this one
            crate::latstat::record("d.queue_depth", (rx.len() + rx_bulk.len()) as f64);
            let name = match &value {
                Value::Map(m) => match proto::msg_type(m) {
                    "pty.data" => "d.write.pty",
                    "screen.frame" => "d.write.screen",
                    "pong" => "d.write.pong",
                    _ => "d.write.other",
                },
                _ => "d.write.other",
            };
            let t0 = std::time::Instant::now();
            if proto::write_frame(&mut wr, &value).await.is_err() {
                break;
            }
            // time for the socket to accept the frame — grows when the link is saturated
            crate::latstat::record(name, crate::latstat::ms(t0.elapsed()));
        }
    });

    let result = frame_loop(&mut rd, &tx, &tx_bulk, peer).await;
    drop(tx);
    drop(tx_bulk);
    let _ = writer.await;
    result
}

async fn frame_loop<R>(
    rd: &mut R,
    tx: &mpsc::Sender<Value>,
    tx_bulk: &mpsc::Sender<Value>,
    peer: SocketAddr,
) -> Result<()>
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
    crate::plog!("[{peer}] paired device connected: {device}");

    let mut caps = vec!["pty", "session", "proc", "metrics", "clip", "sync", "sync_hash", "echo_ack", "keyhold", "chess"];
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
    let mut metrics = Metrics::new();
    let input = Input::new();
    let mut clip_watches: HashMap<i64, clip::Watch> = HashMap::new();
    let clip_last = clip::new_last_seen();
    let mut puts: HashMap<i64, PutState> = HashMap::new();
    let mut chess = crate::chess::wire::Conn::new();

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
                        crate::plog!("[{peer}] pty.open ch={ch} -> session {} {cols}x{rows}", sess.id);
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
                crate::plog!("[{peer}] session.open ch={ch} -> session {} ({})", i.id, i.name);
            }
            "session.rename" => {
                let id = proto::get_i64(&frame, "id").unwrap_or(0) as u64;
                if let (Some(s), Some(name)) = (sessions().get(id), proto::get_str(&frame, "name")) {
                    s.rename(name);
                }
                tx.send(proto::session_list(ch, sessions().list())).await.ok();
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
                        s.mark_input();
                        s.feed(data);
                        if let Some(seq) = proto::get_i64(&frame, "seq") {
                            s.note_input(seq as u64);
                        }
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
                    let ack = proto::get_bool(&frame, "ack");
                    let tiles = proto::get_bool(&frame, "tiles");
                    // A restart on the same channel: stop the old capture first so
                    // two desktop duplications never overlap.
                    screens.remove(&ch);
                    screens.insert(ch, ScreenStream::start(ch, max_w, fps, cursor, ack, tiles, tx_bulk.clone()));
                    crate::plog!("[{peer}] screen.start ch={ch} max_w={max_w} fps={fps} cursor={cursor} ack={ack} tiles={tiles}");
                }
            }
            "screen.stop" => {
                screens.remove(&ch);
            }
            "screen.ack" => {
                if let Some(s) = screens.get(&ch) {
                    s.ack();
                }
            }
            "input.move" | "input.point" | "input.click" | "input.down" | "input.up"
            | "input.key" | "input.keydown" | "input.keyup" | "input.zoom" => {
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
                crate::plog!("[{peer}] proc.kill pid={pid} sig={sig} ok={ok}");
                tx.send(proto::proc_killed(ch, pid, ok)).await.ok();
            }
            "stats.get" => {
                let v = metrics.stats_snapshot();
                tx.send(proto::stats(ch, v)).await.ok();
            }
            "net.get" => {
                let v = metrics.net_snapshot();
                tx.send(proto::net(ch, v)).await.ok();
            }
            "disk.get" => {
                let v = metrics.disk_snapshot();
                tx.send(proto::disk(ch, v)).await.ok();
            }
            "clip.watch" => {
                clip_watches.insert(ch, clip::Watch::start(ch, tx.clone(), clip_last.clone()));
                if let Some(text) = clip::get_text() {
                    tx.send(proto::clip(ch, &text)).await.ok();
                }
            }
            "clip.stop" => {
                clip_watches.remove(&ch);
            }
            "clip.get" => {
                let text = clip::get_text().unwrap_or_default();
                tx.send(proto::clip(ch, &text)).await.ok();
            }
            "clip.set" => {
                if let Some(text) = proto::get_str(&frame, "text") {
                    clip::set_text(text, &clip_last);
                }
            }
            // Listing, hashing and deleting can take long (big trees, slow or
            // network drives): each runs on its own task and replies when done,
            // so this loop keeps answering pings and other channels meanwhile.
            "fs.list" => {
                let path = proto::get_str(&frame, "path").unwrap_or_default().to_string();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Ok(v) = tokio::task::spawn_blocking(move || filesync::fs_list(ch, path)).await {
                        tx.send(v).await.ok();
                    }
                });
            }
            "sync.list" => {
                let root = proto::get_str(&frame, "root").unwrap_or_default().to_string();
                let hash = proto::get_bool(&frame, "hash");
                let tx = tx.clone();
                tokio::spawn(async move {
                    let t0 = std::time::Instant::now();
                    let r = root.clone();
                    if let Ok(entries) = tokio::task::spawn_blocking(move || filesync::sync_list(r, hash)).await {
                        crate::plog!("sync.list {root}: {} files in {:?}", entries.len(), t0.elapsed());
                        tx.send(proto::sync_list(ch, entries)).await.ok();
                    }
                });
            }
            "sync.hash" => {
                let root = proto::get_str(&frame, "root").unwrap_or_default().to_string();
                let paths: Vec<String> = proto::get(&frame, "paths")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|p| p.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let tx = tx.clone();
                crate::plog!("sync.hash {root}: {} files", paths.len());
                tokio::task::spawn_blocking(move || filesync::sync_hash(ch, root, paths, tx));
            }
            "sync.put.begin" => {
                let path = proto::get_str(&frame, "path").unwrap_or_default().to_string();
                let mtime = proto::get_i64(&frame, "mtime_ms");
                let outcome = tokio::task::spawn_blocking(move || PutState::begin(&path, mtime)).await;
                match outcome {
                    Ok(Ok((state, resume_offset))) => {
                        puts.insert(ch, state);
                        tx.send(proto::sync_put_ready(ch, resume_offset)).await.ok();
                    }
                    _ => {
                        tx.send(chan_error(ch, "sync.put.begin failed")).await.ok();
                    }
                }
            }
            "sync.put.chunk" => {
                if let (Some(state), Some(offset), Some(data)) = (
                    puts.get_mut(&ch),
                    proto::get_i64(&frame, "offset"),
                    proto::get_bin(&frame, "data"),
                ) {
                    // Bounded to CHUNK_SIZE (256 KiB) by the client — a plain
                    // in-loop write is cheap enough not to need spawn_blocking,
                    // same as the small synchronous fs calls already used above.
                    let tw = std::time::Instant::now();
                    let wrote = state.write_chunk(offset.max(0) as u64, data);
                    crate::latstat::record("d.sync.put_chunk_write_ms", crate::latstat::ms(tw.elapsed()));
                    if wrote.is_err() {
                        puts.remove(&ch);
                        tx.send(chan_error(ch, "sync.put.chunk failed")).await.ok();
                    }
                }
            }
            "sync.put.end" => {
                if let Some(state) = puts.remove(&ch) {
                    let ok = tokio::task::spawn_blocking(move || state.finish())
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false);
                    tx.send(proto::sync_put_done(ch, ok, if ok { None } else { Some("write failed") }))
                        .await
                        .ok();
                }
            }
            "sync.delete" => {
                let path = proto::get_str(&frame, "path").unwrap_or_default().to_string();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let ok = tokio::task::spawn_blocking(move || filesync::delete(&path)).await.unwrap_or(false);
                    tx.send(proto::sync_delete_done(ch, ok)).await.ok();
                });
            }
            "sync.get.begin" => {
                let path = proto::get_str(&frame, "path").unwrap_or_default().to_string();
                let resume_offset = proto::get_i64(&frame, "resume_offset").unwrap_or(0).max(0) as u64;
                tokio::spawn(filesync::send_file(ch, path, resume_offset, tx_bulk.clone()));
            }
            t if t.starts_with("chess.") => {
                chess.handle(t, &frame, ch, tx).await;
            }
            other => {
                crate::plog!("[{peer}] ignoring {other}");
            }
        }
    }

    // Client gone: drop chess subscriptions (jobs keep running in the registry).
    chess.shutdown();

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
