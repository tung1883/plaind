//! Real-socket end-to-end for the chess jobs: a phone connects, starts a job on real Stockfish,
//! drops off the network mid-run, and later reconnects to find the job kept running and every
//! result still waiting for it.

use plaind::chess::install;
use plaind::pairing;
use plaind::proto::{self, s};
use rmpv::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

fn ensure_engine() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("chess-test-data");
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("PLAIND_DATA", &dir);
    if install::find().is_none() {
        install::install(&mut |_, _| {}).expect("install stockfish");
    }
}

fn get<'a>(f: &'a proto::Frame, k: &str) -> Option<&'a Value> {
    proto::get(f, k)
}

async fn connect(port: u16, token: &str) -> (TcpStream, proto::Frame) {
    let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    proto::write_frame(
        &mut c,
        &proto::map(vec![
            ("t", s("hello")),
            ("proto", Value::from(proto::PROTO)),
            ("token", s(token)),
            ("device", s("rig")),
        ]),
    )
    .await
    .unwrap();
    let w = proto::read_frame(&mut c).await.unwrap().unwrap();
    assert_eq!(proto::msg_type(&w), "welcome");
    (c, w)
}

async fn send(c: &mut TcpStream, t: &str, mut kv: Vec<(&str, Value)>) {
    kv.insert(0, ("t", s(t)));
    kv.push(("ch", Value::from(5)));
    proto::write_frame(c, &proto::map(kv)).await.unwrap();
}

fn game_value(i: usize) -> Value {
    let sans = ["Nf3", "Nf6", "g3", "g6", "Bg2", "Bg7", "O-O", "O-O", "d3", "d6"];
    proto::map(vec![
        ("id", s(&format!("tcp{i}"))),
        ("src", s("t.pgn")),
        ("white", s("W")),
        ("black", s("B")),
        ("event", s("E")),
        ("date", s("2020.01.01")),
        ("sans", Value::Array(sans.iter().map(|x| s(x)).collect())),
    ])
}

/// Reads frames until `pred` says stop; returns everything seen. Panics on timeout.
async fn read_until(
    c: &mut TcpStream,
    secs: u64,
    mut pred: impl FnMut(&proto::Frame) -> bool,
) -> Vec<proto::Frame> {
    let mut seen = Vec::new();
    timeout(Duration::from_secs(secs), async {
        loop {
            let f = proto::read_frame(c).await.unwrap().expect("socket closed");
            let stop = pred(&f);
            seen.push(f);
            if stop {
                return;
            }
        }
    })
    .await
    .expect("timed out");
    seen
}

fn result_seqs(frames: &[proto::Frame]) -> Vec<i64> {
    let mut out = Vec::new();
    for f in frames {
        if proto::msg_type(f) == "chess.results" {
            if let Some(Value::Array(items)) = get(f, "items") {
                for it in items {
                    if let Value::Map(m) = it {
                        if let Some((_, v)) = m.iter().find(|(k, _)| k.as_str() == Some("seq")) {
                            out.push(v.as_i64().unwrap());
                        }
                    }
                }
            }
        }
    }
    out
}

fn job_scanned(list: &proto::Frame, job: &str) -> Option<i64> {
    let Some(Value::Array(jobs)) = get(list, "jobs") else { return None };
    for j in jobs {
        if let Value::Map(m) = j {
            let g = |k: &str| m.iter().find(|(kk, _)| kk.as_str() == Some(k)).map(|(_, v)| v.clone());
            if g("job").and_then(|v| v.as_str().map(String::from)).as_deref() == Some(job) {
                return g("scanned").and_then(|v| v.as_i64());
            }
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn job_survives_phone_disconnect_and_replays_on_reconnect() {
    ensure_engine();
    let token = "chess-tcp-token";
    pairing::trust(token).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            tokio::spawn(async move { plaind::session::handle(stream, peer).await });
        }
    });
    let job = format!("tcp-{}", std::process::id());
    const N: usize = 10;

    // --- phone connects, starts, uploads, attaches ---
    let (mut c, welcome) = connect(port, token).await;
    let caps: Vec<&str> = match get(&welcome, "caps") {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).collect(),
        _ => vec![],
    };
    assert!(caps.contains(&"chess"), "welcome advertises chess: {caps:?}");

    send(&mut c, "chess.status", vec![]).await;
    let st = read_until(&mut c, 20, |f| proto::msg_type(f) == "chess.status").await;
    assert_eq!(get(st.last().unwrap(), "installed").and_then(|v| v.as_bool()), Some(true));

    send(
        &mut c,
        "chess.job.start",
        vec![
            ("job", s(&job)),
            ("total", Value::from(N as i64)),
            ("workers", Value::from(1)),
            ("params", proto::map(vec![("walk_depth", Value::from(12)), ("walk_cap_ms", Value::from(1500))])),
        ],
    )
    .await;
    read_until(&mut c, 10, |f| proto::msg_type(f) == "chess.job.started").await;
    send(&mut c, "chess.games", vec![("job", s(&job)), ("games", Value::Array((0..N).map(game_value).collect()))]).await;
    read_until(&mut c, 10, |f| proto::msg_type(f) == "chess.games.ack").await;
    send(&mut c, "chess.games.end", vec![("job", s(&job))]).await;
    send(&mut c, "chess.attach", vec![("job", s(&job)), ("after", Value::from(0))]).await;

    // --- first couple of results arrive live, then the phone vanishes (no ack, no detach) ---
    let mut got = 0usize;
    let first = read_until(&mut c, 120, |f| {
        got += result_seqs(std::slice::from_ref(f)).len();
        got >= 2
    })
    .await;
    let seen_live = result_seqs(&first);
    assert!(seen_live.len() >= 2 && seen_live.len() < N, "disconnect happens mid-run: {seen_live:?}");
    drop(c); // abrupt TCP close

    // --- while disconnected, the job keeps going (probe from a second connection) ---
    let (mut probe, _) = connect(port, token).await;
    let t0 = Instant::now();
    let mut scanned_later = 0;
    while t0.elapsed() < Duration::from_secs(120) {
        send(&mut probe, "chess.job.list", vec![]).await;
        let l = read_until(&mut probe, 10, |f| proto::msg_type(f) == "chess.job.list").await;
        scanned_later = job_scanned(l.last().unwrap(), &job).unwrap_or(0);
        if scanned_later as usize == N {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(scanned_later as usize, N, "job finished with nobody attached");

    // --- reconnect: everything unacked is replayed, ending with a terminal progress ---
    send(&mut probe, "chess.attach", vec![("job", s(&job)), ("after", Value::from(0))]).await;
    let all = read_until(&mut probe, 20, |f| {
        proto::msg_type(f) == "chess.progress" && proto::get_str(f, "state") != Some("running")
    })
    .await;
    let mut seqs = result_seqs(&all);
    seqs.sort();
    assert_eq!(seqs, (1..=N as i64).collect::<Vec<_>>(), "every result replayed, none lost or duplicated");
    for seq in &seen_live {
        assert!(seqs.contains(seq));
    }
    assert_eq!(proto::get_str(all.last().unwrap(), "state"), Some("done"));

    // --- ack + remove ---
    send(&mut probe, "chess.ack", vec![("job", s(&job)), ("upto", Value::from(N as i64))]).await;
    send(&mut probe, "chess.job.remove", vec![("job", s(&job))]).await;
    let rm = read_until(&mut probe, 10, |f| proto::msg_type(f) == "chess.job.removed").await;
    assert_eq!(get(rm.last().unwrap(), "ok").and_then(|v| v.as_bool()), Some(true));
}
