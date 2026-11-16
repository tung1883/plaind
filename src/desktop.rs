//! Screen and input for one client connection.
//!
//! The daemon normally captures and injects in its own process ([`LocalDesktop`]). On Windows a
//! desktop agent (`agent.rs`) may be running with more privilege than the daemon; when its pipe
//! is there, the client's `screen.*` / `input.*` frames are passed to it unchanged and the frames
//! it produces are relayed back.

use rmpv::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::input::Input;
use crate::proto::{self, Frame};
use crate::screen::ScreenStream;

/// How long a lost agent is waited for before the work moves into this process.
const AGENT_WAIT: Duration = Duration::from_secs(5);

/// Capture and injection done right here, in this process.
pub struct LocalDesktop {
    who: String,
    screens: HashMap<i64, ScreenStream>,
    input: Input,
}

impl LocalDesktop {
    /// `who` prefixes this desktop's log lines.
    pub fn new(who: String) -> Self {
        LocalDesktop { who, screens: HashMap::new(), input: Input::new() }
    }

    /// Act on one `screen.*` / `input.*` frame. Screen frames go out on `tx_bulk`; a returned
    /// value is an error for the client.
    pub fn handle(&mut self, frame: &Frame, tx_bulk: &Sender<Value>) -> Option<Value> {
        let ch = proto::get_i64(frame, "ch").unwrap_or(-1);
        match proto::msg_type(frame) {
            "screen.start" => {
                if !ScreenStream::SUPPORTED {
                    return Some(proto::chan_error(ch, "screen not supported on this host"));
                }
                let max_w = proto::get_i64(frame, "max_w").unwrap_or(1280) as u32;
                let fps = proto::get_i64(frame, "fps").unwrap_or(5);
                let cursor = proto::get(frame, "cursor").and_then(|v| v.as_bool()).unwrap_or(true);
                let ack = proto::get_bool(frame, "ack");
                let tiles = proto::get_bool(frame, "tiles");
                // A restart on the same channel: stop the old capture first so
                // two desktop duplications never overlap.
                self.screens.remove(&ch);
                self.screens.insert(ch, ScreenStream::start(ch, max_w, fps, cursor, ack, tiles, tx_bulk.clone()));
                crate::plog!(
                    "{} screen.start ch={ch} max_w={max_w} fps={fps} cursor={cursor} ack={ack} tiles={tiles}",
                    self.who
                );
            }
            "screen.stop" => {
                self.screens.remove(&ch);
            }
            "screen.ack" => {
                if let Some(s) = self.screens.get(&ch) {
                    s.ack();
                }
            }
            _ => self.input.handle(frame),
        }
        None
    }
}

/// A connection's screen and input, wherever they are carried out: a queue into a task that
/// owns the real thing, so the connection loop never waits on it.
pub struct Desktop {
    tx: Sender<Value>,
}

impl Desktop {
    /// `reply` takes errors for the client, `bulk` takes screen frames.
    pub fn spawn(who: String, reply: Sender<Value>, bulk: Sender<Value>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(run(who, rx, reply, bulk));
        Desktop { tx }
    }

    pub async fn forward(&self, frame: Frame) {
        let _ = self.tx.send(Value::Map(frame)).await;
    }
}

enum Mode {
    /// Nothing asked for yet.
    Idle,
    Local(LocalDesktop),
    Remote(remote::Link),
}

async fn run(who: String, mut rx: Receiver<Value>, reply: Sender<Value>, bulk: Sender<Value>) {
    // The screen streams the client currently wants, so they can be started again elsewhere.
    let mut starts: HashMap<i64, Value> = HashMap::new();
    let mut mode = Mode::Idle;
    loop {
        let next = match &mut mode {
            Mode::Remote(link) => tokio::select! {
                v = rx.recv() => v,
                _ = link.lost() => {
                    crate::plog!("{who} desktop agent went away");
                    match recover(&who, &mut rx, &mut starts, &reply, &bulk).await {
                        Some(m) => mode = m,
                        None => return,
                    }
                    continue;
                }
            },
            _ => rx.recv().await,
        };
        let Some(value) = next else { return };
        let Value::Map(frame) = &value else { continue };
        let starting = proto::msg_type(frame) == "screen.start";
        track(&mut starts, frame, &value);

        // Look for an agent before the first use, and again whenever a stream starts while the
        // work is local: that is when one installed or restarted since gets picked up.
        let look = match mode {
            Mode::Idle => true,
            Mode::Local(_) => starting,
            Mode::Remote(_) => false,
        };
        if look {
            if let Some(mut link) = remote::Link::connect(&bulk).await {
                crate::plog!("{who} screen and input through the desktop agent");
                for start in starts.values() {
                    link.send(start).await;
                }
                mode = Mode::Remote(link);
                if starting {
                    continue; // it was among the streams just started
                }
            } else if matches!(mode, Mode::Idle) {
                mode = Mode::Local(LocalDesktop::new(who.clone()));
            }
        }

        match &mut mode {
            Mode::Local(desk) => {
                if let Some(err) = desk.handle(frame, &bulk) {
                    let _ = reply.send(err).await;
                }
            }
            // A failed write means the pipe broke; `lost` reports it on the next turn.
            Mode::Remote(link) => {
                link.send(&value).await;
            }
            Mode::Idle => {}
        }
    }
}

fn track(starts: &mut HashMap<i64, Value>, frame: &Frame, value: &Value) {
    let ch = proto::get_i64(frame, "ch").unwrap_or(-1);
    match proto::msg_type(frame) {
        "screen.start" => {
            starts.insert(ch, value.clone());
        }
        "screen.stop" => {
            starts.remove(&ch);
        }
        _ => {}
    }
}

/// The agent's pipe broke. It is normally back within a second or two (the service restarts
/// it), so wait a little, remembering which streams the client wants meanwhile, and start them
/// again on whichever side ends up doing the work. `None`: the client left first.
async fn recover(
    who: &str,
    rx: &mut Receiver<Value>,
    starts: &mut HashMap<i64, Value>,
    reply: &Sender<Value>,
    bulk: &Sender<Value>,
) -> Option<Mode> {
    let give_up = tokio::time::Instant::now() + AGENT_WAIT;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            v = rx.recv() => {
                let value = v?;
                if let Value::Map(frame) = &value {
                    track(starts, frame, &value);
                }
            }
            _ = tick.tick() => {
                if let Some(mut link) = remote::Link::connect(bulk).await {
                    crate::plog!("{who} desktop agent is back");
                    for start in starts.values() {
                        link.send(start).await;
                    }
                    return Some(Mode::Remote(link));
                }
                if tokio::time::Instant::now() >= give_up {
                    crate::plog!("{who} no desktop agent: screen and input in this process");
                    let mut desk = LocalDesktop::new(who.to_string());
                    for start in starts.values() {
                        if let Value::Map(frame) = start {
                            if let Some(err) = desk.handle(frame, bulk) {
                                let _ = reply.send(err).await;
                            }
                        }
                    }
                    return Some(Mode::Local(desk));
                }
            }
        }
    }
}

#[cfg(windows)]
mod remote {
    use rmpv::Value;
    use std::os::windows::io::AsRawHandle;
    use std::time::Duration;
    use tokio::io::WriteHalf;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
    use tokio::sync::mpsc::Sender;
    use tokio::task::JoinHandle;

    use crate::proto;

    /// A live pipe to the desktop agent.
    pub struct Link {
        wr: WriteHalf<NamedPipeClient>,
        reader: JoinHandle<()>,
    }

    impl Link {
        /// `None` when no agent is reachable — the usual case without the service installed.
        pub async fn connect(bulk: &Sender<Value>) -> Option<Link> {
            let (name, pinned) = crate::agent::pipe_name();
            let mut pipe = open(&name).await?;
            // Anyone can create a pipe with this name before the agent does. Only talk to one
            // made by SYSTEM or an administrator, or keystrokes could go to an impostor.
            if !pinned && !crate::windesk::owner_is_privileged(pipe.as_raw_handle().cast()) {
                crate::plog!("desktop agent pipe {name} was not created by SYSTEM: ignoring it");
                return None;
            }
            let hello = tokio::time::timeout(Duration::from_secs(1), proto::read_frame(&mut pipe))
                .await
                .ok()?
                .ok()??;
            if proto::msg_type(&hello) != "agent.hello" {
                return None;
            }
            if proto::get_i64(&hello, "proto") != Some(crate::agent::AGENT_PROTO) {
                crate::plog!("desktop agent is a different version: run `plaind service install` again");
                return None;
            }
            let (mut rd, wr) = tokio::io::split(pipe);
            let bulk = bulk.clone();
            // Relayed frames are never dropped here: partial screen updates build on each other.
            // A full queue pushes back through the pipe to the agent, which then skips a frame
            // knowing that it did.
            let reader = tokio::spawn(async move {
                while let Ok(Some(frame)) = proto::read_frame(&mut rd).await {
                    if bulk.send(Value::Map(frame)).await.is_err() {
                        break;
                    }
                }
            });
            Some(Link { wr, reader })
        }

        pub async fn send(&mut self, value: &Value) -> bool {
            proto::write_frame(&mut self.wr, value).await.is_ok()
        }

        /// Resolves once the agent's end is gone.
        pub async fn lost(&mut self) {
            let _ = (&mut self.reader).await;
        }
    }

    impl Drop for Link {
        fn drop(&mut self) {
            self.reader.abort();
        }
    }

    async fn open(name: &str) -> Option<NamedPipeClient> {
        const ERROR_PIPE_BUSY: i32 = 231;
        for _ in 0..3 {
            match ClientOptions::new().open(name) {
                Ok(pipe) => return Some(pipe),
                // every instance is taken for a moment; the agent adds one per client
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(_) => return None,
            }
        }
        None
    }
}

#[cfg(not(windows))]
mod remote {
    use rmpv::Value;
    use tokio::sync::mpsc::Sender;

    /// There is no desktop agent off Windows, so this is never built.
    pub struct Link(std::convert::Infallible);

    impl Link {
        pub async fn connect(_bulk: &Sender<Value>) -> Option<Link> {
            None
        }

        pub async fn send(&mut self, _value: &Value) -> bool {
            match self.0 {}
        }

        pub async fn lost(&mut self) {
            match self.0 {}
        }
    }
}
