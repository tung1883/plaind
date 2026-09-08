//! The `input` channel — mouse and keyboard injection. Runs on one dedicated
//! thread because the backend handle is not `Send`.

use crate::proto::Frame;
#[cfg(feature = "input")]
use crate::proto::{get_bool, get_f64, get_str, msg_type};

pub struct Input {
    #[cfg(feature = "input")]
    tx: std::sync::mpsc::Sender<Cmd>,
}

#[cfg(feature = "input")]
enum Cmd {
    Move(f64, f64, f64),
    Click(String, bool),
    Press(bool),
    Key(Option<String>, Option<String>),
}

impl Input {
    pub const SUPPORTED: bool = cfg!(feature = "input");

    #[cfg(feature = "input")]
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        std::thread::spawn(move || {
            use enigo::{Axis, Button, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};
            let mut enigo = match Enigo::new(&Settings::default()) {
                Ok(e) => e,
                Err(_) => return,
            };
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    Cmd::Move(dx, dy, scroll) => {
                        if dx != 0.0 || dy != 0.0 {
                            let _ = enigo.move_mouse(dx as i32, dy as i32, Coordinate::Rel);
                        }
                        if scroll != 0.0 {
                            let _ = enigo.scroll(scroll as i32, Axis::Vertical);
                        }
                    }
                    Cmd::Click(button, double) => {
                        let b = match button.as_str() {
                            "r" => Button::Right,
                            "m" => Button::Middle,
                            _ => Button::Left,
                        };
                        let _ = enigo.button(b, Direction::Click);
                        if double {
                            let _ = enigo.button(b, Direction::Click);
                        }
                    }
                    Cmd::Press(down) => {
                        let _ = enigo.button(
                            Button::Left,
                            if down { Direction::Press } else { Direction::Release },
                        );
                    }
                    Cmd::Key(text, key) => {
                        if let Some(t) = text {
                            let _ = enigo.text(&t);
                        } else if let Some(k) = key {
                            press_named(&mut enigo, &k);
                        }
                    }
                }
            }
        });
        Input { tx }
    }

    #[cfg(not(feature = "input"))]
    pub fn new() -> Self {
        Input {}
    }

    pub fn handle(&self, frame: &Frame) {
        #[cfg(feature = "input")]
        {
            let cmd = match msg_type(frame) {
                "input.move" => Some(Cmd::Move(
                    get_f64(frame, "dx").unwrap_or(0.0),
                    get_f64(frame, "dy").unwrap_or(0.0),
                    get_f64(frame, "scroll").unwrap_or(0.0),
                )),
                "input.click" => Some(Cmd::Click(
                    get_str(frame, "button").unwrap_or("l").to_string(),
                    get_bool(frame, "double"),
                )),
                "input.down" => Some(Cmd::Press(true)),
                "input.up" => Some(Cmd::Press(false)),
                "input.key" => Some(Cmd::Key(
                    get_str(frame, "text").map(String::from),
                    get_str(frame, "key").map(String::from),
                )),
                _ => None,
            };
            if let Some(cmd) = cmd {
                let _ = self.tx.send(cmd);
            }
        }
        #[cfg(not(feature = "input"))]
        let _ = frame;
    }
}

#[cfg(feature = "input")]
fn press_named(enigo: &mut enigo::Enigo, name: &str) {
    use enigo::{Direction, Key, Keyboard};
    let lower = name.to_lowercase();
    if lower == "ctrl-alt-delete" {
        let _ = enigo.key(Key::Control, Direction::Press);
        let _ = enigo.key(Key::Alt, Direction::Press);
        let _ = enigo.key(Key::Delete, Direction::Click);
        let _ = enigo.key(Key::Alt, Direction::Release);
        let _ = enigo.key(Key::Control, Direction::Release);
        return;
    }
    let key = match lower.as_str() {
        "enter" | "return" => Key::Return,
        "backspace" => Key::Backspace,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "space" => Key::Space,
        _ => return,
    };
    let _ = enigo.key(key, Direction::Click);
}
