//! `plaind` library surface — shared by the daemon binary and its integration
//! test. See `main.rs` for the CLI.

#[macro_use]
pub mod plog;
pub mod autostart;
pub mod input;
pub mod metrics;
pub mod pairing;
pub mod paths;
pub mod proto;
pub mod procs;
pub mod pty;
pub mod screen;
pub mod session;
pub mod tray;
