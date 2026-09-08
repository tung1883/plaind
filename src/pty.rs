//! The `pty` channel — a real login shell behind a pseudo-terminal.

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Sender;

use crate::proto::{pty_data, pty_exit};

pub struct PtyChannel {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
}

impl PtyChannel {
    pub fn open(ch: i64, cols: u16, rows: u16, cmd: Option<&str>, tx: Sender<rmpv::Value>) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize {
            rows: rows.max(4),
            cols: cols.max(20),
            pixel_width: 0,
            pixel_height: 0,
        })?;

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

        let child = pair.slave.spawn_command(builder)?;
        let killer = child.clone_killer();
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

        // Blocking read pump: pty output -> pty.data frames, then pty.exit.
        let exit_tx = tx.clone();
        std::thread::spawn(move || {
            let mut child = child;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if exit_tx.blocking_send(pty_data(ch, &buf[..n])).is_err() {
                            break;
                        }
                    }
                }
            }
            let code = child.wait().map(|s| s.exit_code() as i64).unwrap_or(-1);
            let _ = exit_tx.blocking_send(pty_exit(ch, code));
        });

        Ok(PtyChannel {
            master,
            writer: Arc::new(Mutex::new(writer)),
            killer: Arc::new(Mutex::new(killer)),
        })
    }

    pub fn feed(&self, data: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(data);
            let _ = w.flush();
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(master) = self.master.lock() {
            let _ = master.resize(PtySize {
                rows: rows.max(4),
                cols: cols.max(20),
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }
}

impl Drop for PtyChannel {
    fn drop(&mut self) {
        if let Ok(mut k) = self.killer.lock() {
            let _ = k.kill();
        }
    }
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
