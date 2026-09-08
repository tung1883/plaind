//! End-to-end: spin up the session handler on a loopback port, run the phone
//! side of the handshake, open a PTY and prove a command's output comes back.

use plaind::pairing;
use plaind::proto::{self, s};
use rmpv::Value;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

#[tokio::test]
async fn shell_round_trip() {
    let token = "integration-test-token-abc123";
    pairing::trust(token).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        plaind::session::handle(stream, peer).await;
    });

    let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    proto::write_frame(
        &mut client,
        &proto::map(vec![
            ("t", s("hello")),
            ("proto", Value::from(proto::PROTO)),
            ("token", s(token)),
            ("device", s("test-rig")),
        ]),
    )
    .await
    .unwrap();

    let welcome = proto::read_frame(&mut client).await.unwrap().unwrap();
    assert_eq!(proto::msg_type(&welcome), "welcome");
    assert_eq!(proto::get_i64(&welcome, "proto"), Some(proto::PROTO));

    proto::write_frame(
        &mut client,
        &proto::map(vec![
            ("t", s("pty.open")),
            ("ch", Value::from(1)),
            ("cols", Value::from(80)),
            ("rows", Value::from(24)),
            ("cmd", Value::Nil),
        ]),
    )
    .await
    .unwrap();

    // Give the shell a moment to start, then ask it to echo a marker.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let line = if cfg!(windows) {
        "echo PLAINDOK\r\n"
    } else {
        "echo PLAINDOK\n"
    };
    proto::write_frame(&mut client, &proto::pty_data(1, line.as_bytes()))
        .await
        .unwrap();

    let mut seen = String::new();
    let found = timeout(Duration::from_secs(10), async {
        loop {
            let frame = proto::read_frame(&mut client).await.unwrap().unwrap();
            if proto::msg_type(&frame) == "pty.data" {
                if let Some(bytes) = proto::get_bin(&frame, "data") {
                    seen.push_str(&String::from_utf8_lossy(bytes));
                    if seen.contains("PLAINDOK") {
                        return true;
                    }
                }
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(found, "did not see the echoed marker; saw: {seen:?}");
}
