use plaind::pairing;
use plaind::proto::{self, s};
use rmpv::Value;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};

// Needs a real display. Run: cargo test --release --test screentiming -- --ignored --nocapture
#[ignore]
#[tokio::test]
async fn screen_latency_probe() {
    let token = "screen-timing-token";
    pairing::trust(token).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        plaind::session::handle(stream, peer).await;
    });
    let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    proto::write_frame(&mut c, &proto::map(vec![
        ("t", s("hello")), ("proto", Value::from(proto::PROTO)),
        ("token", s(token)), ("device", s("rig")),
    ])).await.unwrap();
    let _ = proto::read_frame(&mut c).await.unwrap().unwrap(); // welcome

    proto::write_frame(&mut c, &proto::map(vec![
        ("t", s("screen.start")), ("ch", Value::from(1)),
        ("max_w", Value::from(1280)), ("fps", Value::from(30)),
        ("cursor", Value::Boolean(false)),
    ])).await.unwrap();

    let start = Instant::now();
    let mut n = 0u32;
    let mut bytes = 0usize;
    let mut last = Instant::now();
    let mut gaps = Vec::new();
    while start.elapsed() < Duration::from_secs(4) {
        let f = proto::read_frame(&mut c).await.unwrap().unwrap();
        if proto::msg_type(&f) == "screen.frame" {
            n += 1;
            if let Some(b) = proto::get_bin(&f, "data") { bytes += b.len(); }
            gaps.push(last.elapsed().as_millis());
            last = Instant::now();
        }
    }
    gaps.sort();
    let med = gaps.get(gaps.len()/2).copied().unwrap_or(0);
    eprintln!("frames {n} in 4s  ~{} fps  avg {}KB  median gap {}ms  max gap {}ms",
        n as f64 / 4.0, bytes / n.max(1) as usize / 1024, med, gaps.last().copied().unwrap_or(0));
}
