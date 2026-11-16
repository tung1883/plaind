//! The desktop agent's pipe, without the service or any privilege: a screen stream asked for
//! over a phone connection comes back through the agent.

#![cfg(all(windows, feature = "screen"))]

use plaind::proto::{self, s};
use plaind::{agent, pairing};
use rmpv::Value;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

fn screen_start(ch: i64) -> Value {
    proto::map(vec![
        ("t", s("screen.start")),
        ("ch", Value::from(ch)),
        ("max_w", Value::from(640)),
        ("fps", Value::from(10)),
        ("cursor", Value::from(false)),
    ])
}

#[tokio::test]
async fn screen_through_the_agent() {
    // One test, so nothing else in this binary sees these.
    let pipe = format!(r"\\.\pipe\plaind-test-{}", std::process::id());
    std::env::set_var("PLAIND_AGENT_PIPE", &pipe);
    std::env::set_var("PLAIND_SYNTH_SCREEN", "low");

    let mut first = start_agent(&pipe);

    // Straight to the pipe, the way the daemon does.
    let mut direct = loop {
        match ClientOptions::new().open(&pipe) {
            Ok(p) => break p,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    let hello = proto::read_frame(&mut direct).await.unwrap().unwrap();
    assert_eq!(proto::msg_type(&hello), "agent.hello");
    assert_eq!(proto::get_i64(&hello, "proto"), Some(agent::AGENT_PROTO));
    proto::write_frame(&mut direct, &screen_start(7)).await.unwrap();
    let frame = timeout(Duration::from_secs(10), proto::read_frame(&mut direct)).await.unwrap().unwrap().unwrap();
    assert_eq!(proto::msg_type(&frame), "screen.frame");
    assert_eq!(proto::get_i64(&frame, "ch"), Some(7));
    drop(direct);

    // And as a phone would see it: the daemon finds the agent and relays its frames.
    let token = "agent-pipe-test-token";
    pairing::trust(token).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        plaind::session::handle(stream, peer).await;
    });
    let mut phone = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let hello = proto::map(vec![
        ("t", s("hello")),
        ("proto", Value::from(proto::PROTO)),
        ("token", s(token)),
        ("device", s("test-rig")),
    ]);
    proto::write_frame(&mut phone, &hello).await.unwrap();
    let welcome = proto::read_frame(&mut phone).await.unwrap().unwrap();
    assert_eq!(proto::msg_type(&welcome), "welcome");

    let before = agent::CLIENTS.load(Ordering::Relaxed);
    proto::write_frame(&mut phone, &screen_start(3)).await.unwrap();
    let frame = timeout(Duration::from_secs(10), proto::read_frame(&mut phone)).await.unwrap().unwrap().unwrap();
    assert_eq!(proto::msg_type(&frame), "screen.frame");
    assert_eq!(proto::get_i64(&frame, "ch"), Some(3));
    assert_eq!(agent::CLIENTS.load(Ordering::Relaxed), before + 1, "the frame should have come from the agent");

    // The agent restarts, as the service makes it do on a logon or after a crash: the stream
    // comes back by itself, with nothing new asked by the phone.
    first.stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _second = start_agent(&pipe);
    let resumed = timeout(Duration::from_secs(30), async {
        // Frames queued before the restart are read off here while the daemon is still waiting
        // for the agent, so a few that arrive after it reconnected can only be new ones.
        let mut fresh = 0;
        while fresh < 3 {
            let frame = proto::read_frame(&mut phone).await.unwrap().unwrap();
            if agent::CLIENTS.load(Ordering::Relaxed) > before + 1 && proto::msg_type(&frame) == "screen.frame" {
                fresh += 1;
            }
        }
    })
    .await;
    assert!(resumed.is_ok(), "the stream should resume through the restarted agent");
}

/// An agent on its own runtime, so it can be taken down like a process that died.
struct Agent(Option<tokio::runtime::Runtime>);

impl Agent {
    fn stop(&mut self) {
        // a runtime may not be dropped from async code; this way of ending it may be
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.stop();
    }
}

fn start_agent(pipe: &str) -> Agent {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let name = pipe.to_string();
    rt.spawn(async move {
        let _ = agent::serve(&name, None).await;
    });
    Agent(Some(rt))
}
