//! Network benchmark for the screen and shell streams, under different link speeds.
//!
//! Runs a private in-process daemon (its own data dir and token, so your real paired
//! devices are untouched), puts a bandwidth + latency limiter between it and a client that
//! behaves like the phone (acks every screen frame after a small decode delay), and measures
//! what the link does to the screen stream and to shell latency.
//!
//!   cargo run --release --example netbench -- [options]
//!
//!   --profiles direct,normal,slow,veryslow   which links (default: all)
//!   --scenarios screen,echo,throughput,mixed which tests (default: all)
//!   --secs 10        screen test length          --fps 12     requested stream fps
//!   --max-w 1280     requested stream width      --decode-ms 6  simulated phone decode time
//!   --source low|high|desktop  what the screen stream shows (default low): low = a few bars
//!                    sliding, high = busy random blocks, both generated in memory so nothing is
//!                    drawn on your desktop; desktop = the real screen (an idle one sends no frames)
//!
//! Profiles (same rate both ways, rtt split evenly): normal 20 Mbit/s 20 ms, slow 2 Mbit/s
//! 120 ms, veryslow 256 kbit/s 400 ms. `direct` = no limiter (loopback).

use plaind::pairing;
use plaind::proto::{self, s};
use rmpv::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{sleep, sleep_until, timeout, Instant};

const TOKEN: &str = "netbench-token";

#[derive(Clone, Copy)]
struct Profile {
    name: &'static str,
    kbps: u64, // 0 = unlimited
    rtt_ms: u64,
}

const PROFILES: [Profile; 4] = [
    Profile { name: "direct", kbps: 0, rtt_ms: 0 },
    Profile { name: "normal", kbps: 20_000, rtt_ms: 20 },
    Profile { name: "slow", kbps: 2_000, rtt_ms: 120 },
    Profile { name: "veryslow", kbps: 256, rtt_ms: 400 },
];

struct Opts {
    profiles: Vec<Profile>,
    scenarios: Vec<String>,
    secs: u64,
    fps: i64,
    max_w: i64,
    decode_ms: u64,
    source: String,
}

fn parse_args() -> Opts {
    let mut o = Opts {
        profiles: PROFILES.to_vec(),
        scenarios: ["screen", "echo", "throughput", "mixed"].iter().map(|x| x.to_string()).collect(),
        secs: 10,
        fps: 12,
        max_w: 1280,
        decode_ms: 6,
        source: "low".to_string(),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let val = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--profiles" => {
                o.profiles = val.split(',').filter_map(|n| PROFILES.iter().find(|p| p.name == n).copied()).collect();
            }
            "--scenarios" => o.scenarios = val.split(',').map(String::from).collect(),
            "--secs" => o.secs = val.parse().unwrap_or(10),
            "--fps" => o.fps = val.parse().unwrap_or(12),
            "--max-w" => o.max_w = val.parse().unwrap_or(1280),
            "--decode-ms" => o.decode_ms = val.parse().unwrap_or(6),
            "--source" => o.source = val,
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
        i += 2;
    }
    o
}

// ---------------------------------------------------------------- link limiter

/// One direction of the limited link. The reader is paced to the link rate (so the sender sees
/// back-pressure like on a real bottleneck); delivery is delayed by the one-way latency.
async fn shape(mut r: OwnedReadHalf, mut w: OwnedWriteHalf, kbps: u64, one_way: Duration) {
    let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
    let writer = tokio::spawn(async move {
        while let Some((at, data)) = rx.recv().await {
            sleep_until(at).await;
            if w.write_all(&data).await.is_err() {
                break;
            }
        }
    });
    // Read big, but model the link packet by packet (1400 bytes): each packet leaves when the
    // link is free and arrives one-way latency later. Sleeping per packet would be hopeless on
    // Windows (coarse timers), so the reader only sleeps once it is 5 ms ahead of the link.
    let mut buf = vec![0u8; 16 * 1024];
    let mut link_free = Instant::now();
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let now = Instant::now();
        for piece in buf[..n].chunks(1400) {
            let start = if link_free > now { link_free } else { now };
            let tx_time = if kbps == 0 { Duration::ZERO } else { Duration::from_micros(piece.len() as u64 * 8 * 1000 / kbps) };
            link_free = start + tx_time;
            let _ = tx.send((link_free + one_way, piece.to_vec()));
        }
        let ahead = Duration::from_millis(5);
        if link_free > Instant::now() + ahead {
            sleep_until(link_free - ahead).await;
        }
    }
    drop(tx);
    let _ = writer.await;
}

/// Listen on a free port; every connection is relayed to `target` through the limiter.
async fn start_proxy(target: std::net::SocketAddr, p: Profile) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else { break };
            let _ = client.set_nodelay(true);
            let Ok(server) = connect_small(target).await else { continue };
            let (cr, cw) = client.into_split();
            let (sr, sw) = server.into_split();
            let one_way = Duration::from_millis(p.rtt_ms / 2);
            tokio::spawn(shape(cr, sw, p.kbps, one_way)); // phone -> daemon
            tokio::spawn(shape(sr, cw, p.kbps, one_way)); // daemon -> phone
        }
    });
    addr
}

/// Small socket buffers, so the OS doesn't hide the limited link behind megabytes of queue.
async fn connect_small(addr: std::net::SocketAddr) -> std::io::Result<TcpStream> {
    let sock = TcpSocket::new_v4()?;
    sock.set_send_buffer_size(32 * 1024)?;
    sock.set_recv_buffer_size(32 * 1024)?;
    let c = sock.connect(addr).await?;
    c.set_nodelay(true)?;
    Ok(c)
}

// ------------------------------------------------------------------- the client

struct FrameRec {
    at: Instant,
    bytes: usize,
    full: bool,
}

struct Client {
    tx: mpsc::UnboundedSender<Value>,
    frames: Arc<Mutex<Vec<FrameRec>>>,
    pty: mpsc::UnboundedReceiver<Vec<u8>>,
}

async fn open_client(addr: std::net::SocketAddr, decode_ms: u64) -> Client {
    let mut c = connect_small(addr).await.expect("connect");
    proto::write_frame(&mut c, &proto::map(vec![
        ("t", s("hello")), ("proto", Value::from(proto::PROTO)), ("token", s(TOKEN)), ("device", s("netbench")),
    ])).await.unwrap();
    let welcome = proto::read_frame(&mut c).await.unwrap().expect("welcome");
    assert_eq!(proto::msg_type(&welcome), "welcome", "daemon refused the bench token");

    let (mut r, mut w) = c.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            if proto::write_frame(&mut w, &v).await.is_err() {
                break;
            }
        }
    });
    let frames = Arc::new(Mutex::new(Vec::<FrameRec>::new()));
    let (pty_tx, pty) = mpsc::unbounded_channel::<Vec<u8>>();
    let (frames2, tx2) = (frames.clone(), tx.clone());
    tokio::spawn(async move {
        loop {
            let f = match proto::read_frame(&mut r).await {
                Ok(Some(f)) => f,
                _ => break,
            };
            match proto::msg_type(&f) {
                "screen.frame" => {
                    let full = proto::get_bool(&f, "full");
                    let mut bytes = 0;
                    if full {
                        bytes = proto::get_bin(&f, "data").map(|b| b.len()).unwrap_or(0);
                    } else if let Some(Value::Array(tiles)) = proto::get(&f, "tiles") {
                        for t in tiles {
                            if let Value::Map(pairs) = t {
                                for (k, v) in pairs {
                                    if k.as_str() == Some("data") {
                                        if let Value::Binary(b) = v {
                                            bytes += b.len();
                                        }
                                    }
                                }
                            }
                        }
                    }
                    frames2.lock().unwrap().push(FrameRec { at: Instant::now(), bytes, full });
                    let ch = proto::get_i64(&f, "ch").unwrap_or(1);
                    let ack_tx = tx2.clone();
                    tokio::spawn(async move {
                        sleep(Duration::from_millis(decode_ms)).await; // the phone's decode time
                        let _ = ack_tx.send(proto::map(vec![("t", s("screen.ack")), ("ch", Value::from(ch))]));
                    });
                }
                "pty.data" => {
                    if let Some(b) = proto::get_bin(&f, "data") {
                        let _ = pty_tx.send(b.to_vec());
                    }
                }
                _ => {}
            }
        }
    });
    Client { tx, frames, pty }
}

fn send(c: &Client, v: Value) {
    let _ = c.tx.send(v);
}

fn screen_start(c: &Client, o: &Opts) {
    send(c, proto::map(vec![
        ("t", s("screen.start")), ("ch", Value::from(1)), ("max_w", Value::from(o.max_w)),
        ("fps", Value::from(o.fps)), ("cursor", Value::Boolean(false)),
        ("ack", Value::Boolean(true)), ("tiles", Value::Boolean(true)),
    ]));
}

fn pty_input(c: &Client, ch: i64, data: &[u8], seq: i64) {
    send(c, proto::map(vec![
        ("t", s("pty.data")), ("ch", Value::from(ch)), ("data", Value::Binary(data.to_vec())), ("seq", Value::from(seq)),
    ]));
}

fn open_shell(c: &Client, ch: i64) {
    send(c, proto::map(vec![
        ("t", s("pty.open")), ("ch", Value::from(ch)), ("cols", Value::from(100)), ("rows", Value::from(30)),
        ("cmd", Value::Nil),
    ]));
}

// ------------------------------------------------------------------ scenarios

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

struct ScreenSummary {
    frames: usize,
    fps: f64,
    avg_kb: f64,
    p95_kb: f64,
    gap_med: f64,
    gap_p95: f64,
    gap_max: f64,
    kbps: f64,
    full: usize,
}

fn summarise_screen(c: &Client, since: Instant, secs: f64) -> ScreenSummary {
    let frames = c.frames.lock().unwrap();
    let recs: Vec<&FrameRec> = frames.iter().filter(|f| f.at >= since).collect();
    let mut kb: Vec<f64> = recs.iter().map(|f| f.bytes as f64 / 1024.0).collect();
    let mut gaps: Vec<f64> = recs.windows(2).map(|w| (w[1].at - w[0].at).as_secs_f64() * 1000.0).collect();
    kb.sort_by(|a, b| a.partial_cmp(b).unwrap());
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let total: f64 = recs.iter().map(|f| f.bytes as f64).sum();
    ScreenSummary {
        frames: recs.len(),
        fps: recs.len() as f64 / secs,
        avg_kb: if recs.is_empty() { 0.0 } else { total / 1024.0 / recs.len() as f64 },
        p95_kb: pct(&kb, 0.95),
        gap_med: pct(&gaps, 0.5),
        gap_p95: pct(&gaps, 0.95),
        gap_max: gaps.last().copied().unwrap_or(0.0),
        kbps: total * 8.0 / 1000.0 / secs,
        full: recs.iter().filter(|f| f.full).count(),
    }
}

async fn run_screen(addr: std::net::SocketAddr, o: &Opts, secs: u64) -> ScreenSummary {
    let c = open_client(addr, o.decode_ms).await;
    screen_start(&c, o);
    let since = Instant::now();
    sleep(Duration::from_secs(secs)).await;
    send(&c, proto::map(vec![("t", s("screen.stop")), ("ch", Value::from(1))]));
    summarise_screen(&c, since, secs as f64)
}

/// Wait until the shell has been quiet for `quiet`.
async fn settle(c: &mut Client, quiet: Duration, max: Duration) {
    let end = Instant::now() + max;
    while Instant::now() < end {
        match timeout(quiet, c.pty.recv()).await {
            Ok(Some(_)) => {}
            _ => return,
        }
    }
}

/// Type letters one at a time and time how long each takes to come back.
async fn echo_latency(c: &mut Client, n: usize) -> Vec<f64> {
    let mut out = Vec::new();
    for i in 0..n {
        let letter = b'a' + (i % 26) as u8;
        while c.pty.try_recv().is_ok() {}
        let t0 = Instant::now();
        pty_input(c, 2, &[letter], i as i64 + 1);
        let deadline = t0 + Duration::from_secs(8);
        'wait: while Instant::now() < deadline {
            match timeout(deadline - Instant::now(), c.pty.recv()).await {
                Ok(Some(d)) if d.contains(&letter) => {
                    out.push(t0.elapsed().as_secs_f64() * 1000.0);
                    break 'wait;
                }
                Ok(Some(_)) => {}
                _ => break 'wait,
            }
        }
        sleep(Duration::from_millis(150)).await;
    }
    out.sort_by(|a, b| a.partial_cmp(b).unwrap());
    out
}

async fn run_echo(addr: std::net::SocketAddr, o: &Opts, with_screen: bool) -> (Vec<f64>, Option<ScreenSummary>) {
    let mut c = open_client(addr, o.decode_ms).await;
    open_shell(&c, 2);
    settle(&mut c, Duration::from_millis(700), Duration::from_secs(10)).await;
    let since = Instant::now();
    if with_screen {
        screen_start(&c, o);
        sleep(Duration::from_secs(2)).await; // let the stream reach steady state
    }
    let t0 = Instant::now();
    let lat = echo_latency(&mut c, 30).await;
    let sum = if with_screen {
        let secs = t0.elapsed().as_secs_f64().max(1.0);
        let r = summarise_screen(&c, t0, secs);
        let _ = since;
        send(&c, proto::map(vec![("t", s("screen.stop")), ("ch", Value::from(1))]));
        Some(r)
    } else {
        None
    };
    (lat, sum)
}

/// A burst of output: how long until the last line arrives, and the effective rate.
async fn run_throughput(addr: std::net::SocketAddr, o: &Opts) -> (usize, f64, bool) {
    let mut c = open_client(addr, o.decode_ms).await;
    open_shell(&c, 2);
    settle(&mut c, Duration::from_millis(700), Duration::from_secs(10)).await;
    while c.pty.try_recv().is_ok() {}
    let pad = "x".repeat(60);
    let cmd = format!("(for /L %i in (1,1,2000) do @echo line %i {pad}) & echo BENCH_DONE\r");
    let t0 = Instant::now();
    pty_input(&c, 2, cmd.as_bytes(), 1);
    let mut buf: Vec<u8> = Vec::new();
    let deadline = t0 + Duration::from_secs(240);
    let mut done = false;
    while Instant::now() < deadline {
        match timeout(deadline - Instant::now(), c.pty.recv()).await {
            Ok(Some(d)) => {
                buf.extend_from_slice(&d);
                // the typed command echoes the marker once; the output is the second one
                if String::from_utf8_lossy(&buf).matches("BENCH_DONE").count() >= 2 {
                    done = true;
                    break;
                }
            }
            _ => break,
        }
    }
    (buf.len(), t0.elapsed().as_secs_f64(), done)
}

// ----------------------------------------------------------------------- main

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
}

#[tokio::main]
async fn main() {
    // Windows sleeps in ~15 ms steps unless asked for finer timers; that would blur every latency below.
    #[cfg(windows)]
    unsafe {
        timeBeginPeriod(1);
    }
    let o = parse_args();
    let dir = std::env::temp_dir().join("plaind-netbench");
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("PLAIND_DATA", &dir); // before anything reads the data dir: keeps your real pairings out of it
    pairing::trust(TOKEN).unwrap();
    plaind::plog::init(); // the daemon's own [lat] / [screen] lines go to <data dir>/plaind.log

    let sock = TcpSocket::new_v4().unwrap();
    sock.set_send_buffer_size(32 * 1024).unwrap();
    sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = sock.listen(16).unwrap();
    let daemon = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else { break };
            let _ = stream.set_nodelay(true);
            tokio::spawn(plaind::session::handle(stream, peer));
        }
    });

    // Screen content comes from the daemon's in-memory synthetic source: nothing is drawn on the
    // desktop and nothing has to be brought to the front. `--source desktop` captures the real one.
    match o.source.as_str() {
        "desktop" => {}
        mode => std::env::set_var("PLAIND_SYNTH_SCREEN", mode),
    }

    println!("netbench: stream {}px @ {} fps requested, decode {} ms, screen test {} s", o.max_w, o.fps, o.decode_ms, o.secs);
    let want = |name: &str| o.scenarios.iter().any(|x| x == name);
    for p in o.profiles.clone() {
        let addr = if p.kbps == 0 { daemon } else { start_proxy(daemon, p).await };
        println!("\n=== {}  ({} kbit/s, rtt {} ms) ===", p.name, if p.kbps == 0 { "unlimited".to_string() } else { p.kbps.to_string() }, p.rtt_ms);
        if want("screen") {
            let r = run_screen(addr, &o, o.secs).await;
            let util = if p.kbps == 0 { String::new() } else { format!("  link use {:.0}%", r.kbps / p.kbps as f64 * 100.0) };
            println!("screen      {:>5.1} fps ({} frames, {} full)  {:>6.1} KB avg {:>6.1} KB p95  gap med {:>5.0} p95 {:>5.0} max {:>5.0} ms  {:>7.0} kbit/s{util}",
                r.fps, r.frames, r.full, r.avg_kb, r.p95_kb, r.gap_med, r.gap_p95, r.gap_max, r.kbps);
            if r.frames == 0 {
                println!("            no frames arrived: the desktop was idle (use --source low|high)");
            }
        }
        if want("echo") {
            let (lat, _) = run_echo(addr, &o, false).await;
            println!("shell echo  med {:>6.0} ms  p95 {:>6.0} ms  max {:>6.0} ms   ({} of 30 keys answered)",
                pct(&lat, 0.5), pct(&lat, 0.95), lat.last().copied().unwrap_or(0.0), lat.len());
        }
        if want("throughput") {
            let (bytes, secs, done) = run_throughput(addr, &o).await;
            println!("shell burst {:>6.1} KB in {:>6.1} s  = {:>6.1} KB/s{}", bytes as f64 / 1024.0, secs, bytes as f64 / 1024.0 / secs.max(0.001),
                if done { "" } else { "   (did not finish)" });
        }
        if want("mixed") {
            let (lat, scr) = run_echo(addr, &o, true).await;
            let scr = scr.unwrap();
            println!("mixed       echo med {:>6.0} ms  p95 {:>6.0} ms  max {:>6.0} ms ({} of 30 keys)  while screen {:>5.1} fps  {:>7.0} kbit/s",
                pct(&lat, 0.5), pct(&lat, 0.95), lat.last().copied().unwrap_or(0.0), lat.len(), scr.fps, scr.kbps);
        }
    }
}
