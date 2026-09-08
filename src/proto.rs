//! The Dev-plugin wire protocol: `u32` big-endian length + a MessagePack map.
//! Mirrors `DevProtocol.java` in the phone app. Canonical version: [`PROTO`].

use rmpv::Value;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const PROTO: i64 = 1;
pub const DEFAULT_PORT: u16 = 8471;
pub const MAX_FRAME: usize = 1 << 20;

/// A decoded frame: always a map, exposed as `(key, value)` pairs.
pub type Frame = Vec<(Value, Value)>;

pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut header = [0u8; 4];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    let value = rmpv::decode::read_value(&mut &body[..])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    match value {
        Value::Map(pairs) => Ok(Some(pairs)),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "frame is not a map")),
    }
}

pub async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, value: &Value) -> io::Result<()> {
    let mut body = Vec::with_capacity(64);
    rmpv::encode::write_value(&mut body, value)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

// --- Frame field access ------------------------------------------------------

pub fn get<'a>(frame: &'a Frame, key: &str) -> Option<&'a Value> {
    frame
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

pub fn get_str<'a>(frame: &'a Frame, key: &str) -> Option<&'a str> {
    get(frame, key).and_then(|v| v.as_str())
}

pub fn get_i64(frame: &Frame, key: &str) -> Option<i64> {
    get(frame, key).and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)))
}

pub fn get_f64(frame: &Frame, key: &str) -> Option<f64> {
    get(frame, key).and_then(|v| {
        v.as_f64()
            .or_else(|| v.as_i64().map(|i| i as f64))
            .or_else(|| v.as_u64().map(|u| u as f64))
    })
}

pub fn get_bool(frame: &Frame, key: &str) -> bool {
    get(frame, key).and_then(|v| v.as_bool()) == Some(true)
}

pub fn get_bin<'a>(frame: &'a Frame, key: &str) -> Option<&'a [u8]> {
    get(frame, key).and_then(|v| match v {
        Value::Binary(b) => Some(b.as_slice()),
        _ => None,
    })
}

pub fn msg_type(frame: &Frame) -> &str {
    get_str(frame, "t").unwrap_or("")
}

// --- Value builders --------------------------------------------------------

pub fn s(v: &str) -> Value {
    Value::String(v.into())
}

pub fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (s(k), v)).collect())
}

pub fn welcome(host: &str, os: &str, caps: &[&str]) -> Value {
    map(vec![
        ("t", s("welcome")),
        ("proto", Value::from(PROTO)),
        ("host", s(host)),
        ("os", s(os)),
        (
            "caps",
            Value::Array(caps.iter().map(|c| s(c)).collect()),
        ),
    ])
}

pub fn error(code: &str, message: &str) -> Value {
    map(vec![
        ("t", s("error")),
        ("code", s(code)),
        ("msg", s(message)),
    ])
}

pub fn pong() -> Value {
    map(vec![("t", s("pong"))])
}

pub fn pty_data(ch: i64, data: &[u8]) -> Value {
    map(vec![
        ("t", s("pty.data")),
        ("ch", Value::from(ch)),
        ("data", Value::Binary(data.to_vec())),
    ])
}

pub fn pty_exit(ch: i64, code: i64) -> Value {
    map(vec![
        ("t", s("pty.exit")),
        ("ch", Value::from(ch)),
        ("code", Value::from(code)),
    ])
}

pub fn screen_frame(ch: i64, w: u32, h: u32, jpeg: Vec<u8>) -> Value {
    map(vec![
        ("t", s("screen.frame")),
        ("ch", Value::from(ch)),
        ("w", Value::from(w)),
        ("h", Value::from(h)),
        ("format", s("jpeg")),
        ("full", Value::Boolean(true)),
        ("data", Value::Binary(jpeg)),
    ])
}

pub fn proc_list(ch: i64, procs: Vec<Value>) -> Value {
    map(vec![
        ("t", s("proc.list")),
        ("ch", Value::from(ch)),
        ("procs", Value::Array(procs)),
    ])
}

pub fn proc_killed(ch: i64, pid: i64, ok: bool) -> Value {
    map(vec![
        ("t", s("proc.killed")),
        ("ch", Value::from(ch)),
        ("pid", Value::from(pid)),
        ("ok", Value::Boolean(ok)),
    ])
}
