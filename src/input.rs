//! The `input` channel — mouse and keyboard injection. Runs on one dedicated
//! thread because the backend handle is not `Send`.

use crate::proto::Frame;
#[cfg(feature = "input")]
use crate::proto::{get, get_bool, get_f64, get_str, msg_type};

pub struct Input {
    #[cfg(feature = "input")]
    tx: std::sync::mpsc::Sender<Cmd>,
}

#[cfg(feature = "input")]
enum Cmd {
    Move(f64, f64, f64),
    Point(f64, f64),
    Click(String, bool),
    Press(bool),
    Key(Option<String>, Option<String>, Vec<String>),
    /// A key held down from the phone: pressed again on every repeat message, released by KeyUp
    /// (or by the watchdog if the phone goes quiet). Modifiers are held around it.
    KeyDown(String, Vec<String>),
    KeyUp,
    Zoom(f64),
}

/// What is currently held down by KeyDown, so it can always be let go.
#[cfg(feature = "input")]
#[derive(Default)]
struct Held {
    key: Option<(enigo::Key, std::time::Instant)>,
    mods: Vec<enigo::Key>,
}

#[cfg(feature = "input")]
impl Held {
    fn release_all(&mut self, enigo: &mut enigo::Enigo) {
        use enigo::{Direction, Keyboard};
        if let Some((k, _)) = self.key.take() {
            let _ = enigo.key(k, Direction::Release);
        }
        for m in self.mods.drain(..).rev() {
            let _ = enigo.key(m, Direction::Release);
        }
    }

    /// The phone sends a repeat every few tens of ms while a key is held; if they stop, let go.
    fn release_if_stale(&mut self, enigo: &mut enigo::Enigo) {
        if let Some((_, at)) = self.key {
            if at.elapsed() > std::time::Duration::from_secs(3) {
                self.release_all(enigo);
            }
        }
    }
}

#[cfg(feature = "input")]
fn modifier_keys(mods: &[String]) -> Vec<enigo::Key> {
    use enigo::Key;
    mods.iter()
        .filter_map(|m| match m.as_str() {
            "ctrl" => Some(Key::Control),
            "alt" => Some(Key::Alt),
            "shift" => Some(Key::Shift),
            "meta" | "win" => Some(Key::Meta),
            _ => None,
        })
        .collect()
}

impl Input {
    pub const SUPPORTED: bool = cfg!(feature = "input");

    #[cfg(feature = "input")]
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        std::thread::spawn(move || {
            #[allow(unused_imports)]
            use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
            let mut enigo = match Enigo::new(&Settings::default()) {
                Ok(e) => e,
                Err(_) => return,
            };
            let mut held = Held::default();
            loop {
                let cmd = match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                    Ok(c) => c,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        held.release_if_stale(&mut enigo);
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };
                match cmd {
                    Cmd::Move(dx, dy, scroll) => {
                        if dx != 0.0 || dy != 0.0 {
                            let _ = enigo.move_mouse(dx as i32, dy as i32, Coordinate::Rel);
                        }
                        if scroll != 0.0 {
                            let _ = enigo.scroll(scroll as i32, Axis::Vertical);
                        }
                    }
                    Cmd::Point(nx, ny) => {
                        point_absolute(nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0));
                    }
                    // Trackpad pinch: Ctrl + wheel, which most apps read as zoom.
                    Cmd::Zoom(ticks) => {
                        let n = ticks.round() as i32;
                        if n != 0 {
                            let _ = enigo.key(Key::Control, Direction::Press);
                            let _ = enigo.scroll(n, Axis::Vertical);
                            let _ = enigo.key(Key::Control, Direction::Release);
                        }
                    }
                    Cmd::Click(button, double) => {
                        crate::plog!("[input] click {button} double={double}");
                        #[cfg(windows)]
                        {
                            raw_button(&button, true);
                            raw_button(&button, false);
                            if double {
                                raw_button(&button, true);
                                raw_button(&button, false);
                            }
                        }
                        #[cfg(not(windows))]
                        {
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
                    }
                    Cmd::Press(down) => {
                        #[cfg(windows)]
                        raw_button("l", down);
                        #[cfg(not(windows))]
                        {
                            let _ = enigo.button(
                                Button::Left,
                                if down { Direction::Press } else { Direction::Release },
                            );
                        }
                    }
                    Cmd::KeyDown(name, mods) => {
                        let Some(key) = named_key(&name.to_lowercase(), &name) else { continue };
                        // a different key than the one held: let that one go first
                        if let Some((k, _)) = held.key {
                            if k != key {
                                let _ = enigo.key(k, Direction::Release);
                                held.key = None;
                            }
                        }
                        for m in modifier_keys(&mods) {
                            if !held.mods.contains(&m) {
                                let _ = enigo.key(m, Direction::Press);
                                held.mods.push(m);
                            }
                        }
                        let _ = enigo.key(key, Direction::Press);   // pressing again is a repeat
                        held.key = Some((key, std::time::Instant::now()));
                    }
                    Cmd::KeyUp => held.release_all(&mut enigo),
                    Cmd::Key(text, key, mods) => {
                        use enigo::{Direction, Key};
                        let held: Vec<Key> = mods
                            .iter()
                            .filter_map(|m| match m.as_str() {
                                "ctrl" => Some(Key::Control),
                                "alt" => Some(Key::Alt),
                                "shift" => Some(Key::Shift),
                                "meta" | "win" => Some(Key::Meta),
                                _ => None,
                            })
                            .collect();
                        for k in &held {
                            let _ = enigo.key(*k, Direction::Press);
                        }
                        if let Some(t) = text {
                            let _ = enigo.text(&t);
                        } else if let Some(k) = key {
                            press_named(&mut enigo, &k);
                        }
                        for k in held.iter().rev() {
                            let _ = enigo.key(*k, Direction::Release);
                        }
                    }
                }
            }
            held.release_all(&mut enigo);   // the connection is gone: nothing stays pressed
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
                "input.zoom" => Some(Cmd::Zoom(get_f64(frame, "ticks").unwrap_or(0.0))),
                "input.point" => Some(Cmd::Point(
                    get_f64(frame, "x").unwrap_or(0.0),
                    get_f64(frame, "y").unwrap_or(0.0),
                )),
                "input.click" => Some(Cmd::Click(
                    get_str(frame, "button").unwrap_or("l").to_string(),
                    get_bool(frame, "double"),
                )),
                "input.down" => Some(Cmd::Press(true)),
                "input.up" => Some(Cmd::Press(false)),
                "input.keydown" => get_str(frame, "key").map(|k| {
                    Cmd::KeyDown(
                        k.to_string(),
                        get(frame, "mods")
                            .and_then(|v| v.as_array())
                            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                            .unwrap_or_default(),
                    )
                }),
                "input.keyup" => Some(Cmd::KeyUp),
                "input.key" => Some(Cmd::Key(
                    get_str(frame, "text").map(String::from),
                    get_str(frame, "key").map(String::from),
                    get(frame, "mods")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                )),
                _ => None,
            };
            if let Some(cmd) = cmd {
                if msg_type(frame) != "input.keydown" {
                    crate::plog!("[input] {}", msg_type(frame));
                }
                let _ = self.tx.send(cmd);
            }
        }
        #[cfg(not(feature = "input"))]
        let _ = frame;
    }
}

/// Move the pointer to a fraction of the primary monitor. Uses an absolute
/// `SendInput` move — unlike `SetCursorPos`, this counts as real mouse input,
/// so Windows un-hides a cursor that was hidden after keyboard typing.
#[cfg(all(feature = "input", windows))]
fn point_absolute(nx: f64, ny: f64) {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
    };
    // Absolute coords are 0..=65535 across the primary monitor.
    let x = (nx.clamp(0.0, 1.0) * 65535.0).round() as i32;
    let y = (ny.clamp(0.0, 1.0) * 65535.0).round() as i32;
    unsafe {
        let mut input: INPUT = zeroed();
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi.dx = x;
        input.Anonymous.mi.dy = y;
        input.Anonymous.mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE;
        SendInput(1, &input, size_of::<INPUT>() as i32);
    }
}

/// Raw mouse button down/up via SendInput — never touches the cursor position,
/// so a click after a tap-to-point leaves the pointer where the tap put it.
#[cfg(windows)]
fn raw_button(button: &str, down: bool) {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    };
    let flag = match (button, down) {
        ("r", true) => MOUSEEVENTF_RIGHTDOWN,
        ("r", false) => MOUSEEVENTF_RIGHTUP,
        ("m", true) => MOUSEEVENTF_MIDDLEDOWN,
        ("m", false) => MOUSEEVENTF_MIDDLEUP,
        (_, true) => MOUSEEVENTF_LEFTDOWN,
        (_, false) => MOUSEEVENTF_LEFTUP,
    };
    unsafe {
        let mut input: INPUT = zeroed();
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi.dwFlags = flag;
        SendInput(1, &input, size_of::<INPUT>() as i32);
    }
}

#[cfg(all(feature = "input", not(windows)))]
fn point_absolute(nx: f64, ny: f64) {
    // non-Windows: fall back through a fresh enigo (rare path)
    use enigo::{Coordinate, Enigo, Mouse, Settings};
    if let Ok(mut e) = Enigo::new(&Settings::default()) {
        let (w, h) = e.main_display().unwrap_or((1920, 1080));
        let _ = e.move_mouse((nx * w as f64) as i32, (ny * h as f64) as i32, Coordinate::Abs);
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
    let Some(key) = named_key(&lower, name) else { return };
    let _ = enigo.key(key, Direction::Click);
}

/// The key for a name from the phone ("enter", "f5", "capslock", "c" ...), or None.
#[cfg(feature = "input")]
fn named_key(lower: &str, original: &str) -> Option<enigo::Key> {
    use enigo::Key;
    let key = match lower {
        "enter" | "return" => Key::Return,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "space" => Key::Space,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        "capslock" | "caps" => Key::CapsLock,
        "win" | "meta" => Key::Meta,
        #[cfg(any(windows, all(unix, not(target_os = "macos"))))]
        "insert" | "ins" => Key::Insert,
        #[cfg(any(windows, all(unix, not(target_os = "macos"))))]
        "printscreen" | "prtsc" => Key::PrintScr,
        #[cfg(windows)]
        "menu" | "apps" => Key::Apps,
        _ => {
            let mut chars = original.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Unicode(c),   // one character: that key
                _ => return None,
            }
        }
    };
    Some(key)
}
