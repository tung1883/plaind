//! OS clipboard access + a change-watcher, for phone<->PC clipboard sync.
//! Mirrors `screen.rs`'s spawned-thread-with-a-stop-flag shape.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::Sender;

use crate::proto::clip;

const POLL: Duration = Duration::from_millis(800);

/// Last text either side is known to hold, shared between a connection's
/// `clip.set` handler and its watch loop so setting the clipboard from the
/// phone doesn't immediately echo back out as a "PC changed it" push.
pub type LastSeen = Arc<Mutex<Option<String>>>;

pub fn new_last_seen() -> LastSeen {
    Arc::new(Mutex::new(None))
}

pub fn get_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

pub fn set_text(text: &str, last: &LastSeen) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_string());
    }
    *last.lock().unwrap() = Some(text.to_string());
}

pub struct Watch {
    stop: Arc<AtomicBool>,
}

impl Watch {
    pub fn start(ch: i64, tx: Sender<rmpv::Value>, last: LastSeen) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        // Seed with whatever's already there so a fresh watch doesn't
        // immediately fire for pre-existing clipboard content.
        {
            let mut l = last.lock().unwrap();
            if l.is_none() {
                *l = get_text();
            }
        }
        std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                if let Some(text) = get_text() {
                    let changed = {
                        let mut l = last.lock().unwrap();
                        if l.as_deref() != Some(text.as_str()) {
                            *l = Some(text.clone());
                            true
                        } else {
                            false
                        }
                    };
                    if changed {
                        match tx.try_send(clip(ch, &text)) {
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                            _ => {}
                        }
                    }
                }
                std::thread::sleep(POLL);
            }
        });
        Watch { stop }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
