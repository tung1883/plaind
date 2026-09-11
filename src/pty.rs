//! Persistent shell sessions behind pseudo-terminals.
//!
//! A session's pty keeps running after its client detaches; recent output is
//! buffered so a reconnecting client can repaint. Sessions live in the
//! process-global [`sessions`] registry, independent of any one connection.

use anyhow::Result;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;

use crate::proto::{pty_data, pty_exit};

/// Bytes of recent output kept for a reconnecting client to repaint from.
const BUFFER_CAP: usize = 256 * 1024;

type Frame = rmpv::Value;

/// The channel a session's output is currently forwarded to (if any client is attached).
struct Bound {
    ch: i64,
    tx: Sender<Frame>,
}

pub struct Session {
    pub id: u64,
    pub name: Mutex<String>,
    /// Ephemeral sessions (the legacy `pty.open` path) die when their channel closes.
    pub ephemeral: bool,
    created_ms: i64,
    cols: AtomicI64,
    rows: AtomicI64,
    alive: Arc<AtomicBool>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    buffer: Mutex<VecDeque<u8>>,
    bound: Mutex<Option<Bound>>,
}

pub struct SessionInfo {
    pub id: u64,
    pub name: String,
    pub cols: i64,
    pub rows: i64,
    pub alive: bool,
    pub created_ms: i64,
}

impl Session {
    /// Feed keystrokes into the shell.
    pub fn feed(&self, data: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(data);
            let _ = w.flush();
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let (c, r) = (cols.max(20), rows.max(4));
        self.cols.store(c as i64, Ordering::Relaxed);
        self.rows.store(r as i64, Ordering::Relaxed);
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize { rows: r, cols: c, pixel_width: 0, pixel_height: 0 });
        }
    }

    /// Bind a channel and return a snapshot of the output buffer for replay.
    pub fn attach(&self, ch: i64, tx: Sender<Frame>) -> Vec<u8> {
        *self.bound.lock().unwrap() = Some(Bound { ch, tx });
        self.buffer.lock().unwrap().iter().copied().collect()
    }

    /// Unbind `ch` (no-op if a different channel is bound); the pty keeps running.
    pub fn detach(&self, ch: i64) {
        let mut b = self.bound.lock().unwrap();
        if b.as_ref().map(|x| x.ch) == Some(ch) {
            *b = None;
        }
    }

    pub fn kill(&self) {
        if let Ok(mut k) = self.killer.lock() {
            let _ = k.kill();
        }
    }

    pub fn rename(&self, name: &str) {
        let n = name.trim();
        if !n.is_empty() {
            *self.name.lock().unwrap() = n.to_string();
        }
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id,
            name: self.name.lock().unwrap().clone(),
            cols: self.cols.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            alive: self.alive.load(Ordering::Relaxed),
            created_ms: self.created_ms,
        }
    }
}

pub struct Sessions {
    map: Mutex<HashMap<u64, Arc<Session>>>,
    next: AtomicU64,
}

/// The process-global session registry.
pub fn sessions() -> &'static Sessions {
    static S: OnceLock<Sessions> = OnceLock::new();
    S.get_or_init(|| Sessions {
        map: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1),
    })
}

impl Sessions {
    pub fn get(&self, id: u64) -> Option<Arc<Session>> {
        self.map.lock().unwrap().get(&id).cloned()
    }

    pub fn remove(&self, id: u64) {
        self.map.lock().unwrap().remove(&id);
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut v: Vec<SessionInfo> =
            self.map.lock().unwrap().values().map(|s| s.info()).collect();
        v.sort_by_key(|i| i.id);
        v
    }

    /// Spawn a shell behind a fresh pty and register the session.
    pub fn create(
        &self,
        name: Option<String>,
        cols: u16,
        rows: u16,
        cmd: Option<&str>,
        ephemeral: bool,
    ) -> Result<Arc<Session>> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let name = name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("shell {id}"));
        let (c, r) = (cols.max(20), rows.max(4));

        let pair = native_pty_system().openpty(PtySize {
            rows: r,
            cols: c,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let child = spawn_shell(&pair, cmd, &name)?;
        let killer = child.clone_killer();
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let alive = Arc::new(AtomicBool::new(true));
        let session = Arc::new(Session {
            id,
            name: Mutex::new(name),
            ephemeral,
            created_ms: now_ms(),
            cols: AtomicI64::new(c as i64),
            rows: AtomicI64::new(r as i64),
            alive: alive.clone(),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            buffer: Mutex::new(VecDeque::new()),
            bound: Mutex::new(None),
        });
        self.map.lock().unwrap().insert(id, session.clone());

        // Read pump: pty output -> ring buffer + the bound channel (if any).
        let s = session.clone();
        std::thread::spawn(move || {
            let mut child = child;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        {
                            let mut b = s.buffer.lock().unwrap();
                            b.extend(chunk.iter().copied());
                            let over = b.len().saturating_sub(BUFFER_CAP);
                            if over > 0 {
                                b.drain(..over);
                            }
                        }
                        if let Some(bd) = s.bound.lock().unwrap().as_ref() {
                            let _ = bd.tx.blocking_send(pty_data(bd.ch, chunk));
                        }
                    }
                }
            }
            alive.store(false, Ordering::Relaxed);
            let code = child.wait().map(|st| st.exit_code() as i64).unwrap_or(-1);
            if let Some(bd) = s.bound.lock().unwrap().as_ref() {
                let _ = bd.tx.blocking_send(pty_exit(bd.ch, code));
            }
            sessions().remove(s.id);
        });

        Ok(session)
    }
}

fn spawn_shell(
    pair: &portable_pty::PtyPair,
    cmd: Option<&str>,
    name: &str,
) -> Result<Box<dyn Child + Send + Sync>> {
    // Optional durability upgrade: wrap each shell in tmux so it survives a
    // daemon restart. Unix only, opt-in via PLAIND_TMUX, silently ignored if
    // tmux is not installed.
    if cmd.is_none() && cfg!(unix) && std::env::var_os("PLAIND_TMUX").is_some() {
        let safe: String = name
            .chars()
            .map(|ch| if ch.is_alphanumeric() { ch } else { '_' })
            .collect();
        let mut b = CommandBuilder::new("tmux");
        b.arg("new-session");
        b.arg("-A");
        b.arg("-s");
        b.arg(format!("plain_{safe}"));
        b.env("TERM", "xterm-256color");
        if let Some(home) = dirs::home_dir() {
            b.cwd(home);
        }
        if let Ok(child) = pair.slave.spawn_command(b) {
            return Ok(child);
        }
    }

    let mut builder = match cmd {
        Some(c) if !c.is_empty() => {
            let mut b = CommandBuilder::new(default_shell());
            b.arg(shell_c_flag());
            b.arg(c);
            b
        }
        _ => CommandBuilder::new(default_shell()),
    };
    builder.env("TERM", "xterm-256color");
    if let Some(home) = dirs::home_dir() {
        builder.cwd(home);
    }
    Ok(pair.slave.spawn_command(builder)?)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("ComSpec").unwrap_or_else(|_| "powershell.exe".to_string())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
    }
}

fn shell_c_flag() -> &'static str {
    if cfg!(windows) {
        "/C"
    } else {
        "-c"
    }
}
