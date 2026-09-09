use plaind::pairing;
use plaind::proto::{self, s};
use rmpv::Value;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

#[tokio::test]
async fn stale_id_replies_session_gone() {
    let token = "gone-test-token";
    pairing::trust(token).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            tokio::spawn(async move { plaind::session::handle(stream, peer).await });
        }
    });
    let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    proto::write_frame(&mut c, &proto::map(vec![
        ("t", s("hello")), ("proto", Value::from(proto::PROTO)),
        ("token", s(token)), ("device", s("rig")),
    ])).await.unwrap();
    let w = proto::read_frame(&mut c).await.unwrap().unwrap();
    assert_eq!(proto::msg_type(&w), "welcome");

    // ask for a session id that was never created
    proto::write_frame(&mut c, &proto::map(vec![
        ("t", s("session.open")), ("ch", Value::from(1)),
        ("id", Value::from(999u64)), ("cols", Value::from(80)), ("rows", Value::from(24)),
    ])).await.unwrap();

    let got = timeout(Duration::from_secs(3), async {
        loop {
            let f = proto::read_frame(&mut c).await.unwrap().unwrap();
            let t = proto::msg_type(&f).to_string();
            if t == "session.gone" {
                assert_eq!(proto::get_i64(&f, "id"), Some(999));
                return true;
            }
            if t == "session.opened" { return false; } // wrong: it created one
        }
    }).await.unwrap_or(false);
    assert!(got, "expected session.gone for unknown id");
}
