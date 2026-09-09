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

/// A `session.open` shell keeps running after the client disconnects, and a
/// reconnecting client reattaching by id gets the buffered output replayed.
#[tokio::test]
async fn session_persists_across_reconnect() {
    let token = "integration-test-token-persist";
    pairing::trust(token).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            tokio::spawn(async move { plaind::session::handle(stream, peer).await });
        }
    });

    async fn hello(client: &mut TcpStream, token: &str) {
        proto::write_frame(
            client,
            &proto::map(vec![
                ("t", s("hello")),
                ("proto", Value::from(proto::PROTO)),
                ("token", s(token)),
                ("device", s("test-rig")),
            ]),
        )
        .await
        .unwrap();
        let welcome = proto::read_frame(client).await.unwrap().unwrap();
        assert_eq!(proto::msg_type(&welcome), "welcome");
    }

    // First client: open a fresh session, run a command, detach.
    let mut a = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    hello(&mut a, token).await;
    proto::write_frame(
        &mut a,
        &proto::map(vec![
            ("t", s("session.open")),
            ("ch", Value::from(1)),
            ("cols", Value::from(80)),
            ("rows", Value::from(24)),
        ]),
    )
    .await
    .unwrap();

    let mut session_id = None;
    timeout(Duration::from_secs(5), async {
        loop {
            let f = proto::read_frame(&mut a).await.unwrap().unwrap();
            if proto::msg_type(&f) == "session.opened" {
                session_id = proto::get_i64(&f, "id");
                return;
            }
        }
    })
    .await
    .unwrap();
    let id = session_id.expect("session.opened carried an id");

    tokio::time::sleep(Duration::from_millis(400)).await;
    let line = if cfg!(windows) { "echo PERSISTOK\r\n" } else { "echo PERSISTOK\n" };
    proto::write_frame(&mut a, &proto::pty_data(1, line.as_bytes())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    proto::write_frame(
        &mut a,
        &proto::map(vec![("t", s("session.detach")), ("ch", Value::from(1))]),
    )
    .await
    .unwrap();
    drop(a);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Second client: reattach by id, expect the buffered output to replay.
    let mut b = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    hello(&mut b, token).await;
    proto::write_frame(
        &mut b,
        &proto::map(vec![
            ("t", s("session.open")),
            ("ch", Value::from(1)),
            ("id", Value::from(id)),
            ("cols", Value::from(80)),
            ("rows", Value::from(24)),
        ]),
    )
    .await
    .unwrap();

    let mut seen = String::new();
    let replayed = timeout(Duration::from_secs(5), async {
        loop {
            let f = proto::read_frame(&mut b).await.unwrap().unwrap();
            if proto::msg_type(&f) == "pty.data" {
                if let Some(bytes) = proto::get_bin(&f, "data") {
                    seen.push_str(&String::from_utf8_lossy(bytes));
                    if seen.contains("PERSISTOK") {
                        return true;
                    }
                }
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(replayed, "reattached session did not replay its buffer; saw: {seen:?}");
}
