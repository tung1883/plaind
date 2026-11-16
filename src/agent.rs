//! The desktop agent: a helper process that captures the screen and injects input for the
//! daemon, and nothing else. The plaind service starts it as SYSTEM inside the session that
//! owns the physical screen, which is what lets it reach elevated windows, UAC prompts and the
//! lock screen.
//!
//! It serves a named pipe. Each client is one of the daemon's phone connections and speaks the
//! phone's own `screen.*` / `input.*` frames, so the handling is the daemon's, unchanged.

#![cfg(windows)]

use anyhow::{bail, Context, Result};
use rmpv::Value;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;
use windows_sys::Win32::System::Pipes::GetNamedPipeClientSessionId;

use crate::desktop::LocalDesktop;
use crate::proto;
use crate::windesk::{self, SecurityDescriptor};

/// Version of the daemon-to-agent pipe. Bump on any change the other side must know about.
pub const AGENT_PROTO: i64 = 1;

/// Clients served so far.
pub static CLIENTS: AtomicUsize = AtomicUsize::new(0);

/// The pipe of the agent for this process's session, and whether `PLAIND_AGENT_PIPE` chose it
/// (tests and development do that).
pub fn pipe_name() -> (String, bool) {
    match std::env::var("PLAIND_AGENT_PIPE") {
        Ok(name) if !name.is_empty() => (name, true),
        _ => (format!(r"\\.\pipe\plaind-desktop-{}", windesk::current_session()), false),
    }
}

/// Run the agent until its pipe fails. `dev` lets it run without being SYSTEM, for trying the
/// pipe out by hand: it then reaches only what its own account can.
pub async fn run(dev: bool) -> Result<()> {
    if !dev && !windesk::is_system() {
        bail!("desktop-agent is started by the plaind service (as SYSTEM); to try it by hand add --dev");
    }
    windesk::follow_input_desktop();
    let session = windesk::current_session();
    let user = if dev { windesk::own_user_sid() } else { windesk::session_user_sid(session) };
    // SYSTEM and administrators own the pipe; whoever is at this desktop may talk to it.
    let mut sddl = String::from("D:P(A;;GA;;;SY)(A;;GA;;;BA)");
    if let Some(sid) = &user {
        sddl.push_str(&format!("(A;;GRGW;;;{sid})"));
    }
    let sd = SecurityDescriptor::from_sddl(&sddl).context("building the pipe's access list")?;
    let (name, _) = pipe_name();
    crate::plog!(
        "agent: session {session}, pipe {name}, open to {}{}",
        user.as_deref().unwrap_or("nobody but SYSTEM"),
        if dev { " (dev)" } else { "" }
    );
    serve(&name, Some(sd)).await
}

/// Accept clients on `name` for good. `sd` is the pipe's access list; `None` leaves the default.
pub async fn serve(name: &str, sd: Option<SecurityDescriptor>) -> Result<()> {
    let session = windesk::current_session();
    let create = |first: bool| -> std::io::Result<NamedPipeServer> {
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first).reject_remote_clients(true);
        match &sd {
            Some(sd) => {
                let mut attrs = sd.attributes();
                unsafe { opts.create_with_security_attributes_raw(name, std::ptr::addr_of_mut!(attrs).cast()) }
            }
            None => opts.create(name),
        }
    };
    let mut server = create(true).with_context(|| format!("creating {name}"))?;
    let mut id = 0u64;
    loop {
        server.connect().await?;
        let client = std::mem::replace(&mut server, create(false)?);
        let mut from = 0u32;
        let known = unsafe { GetNamedPipeClientSessionId(client.as_raw_handle().cast(), &mut from) } != 0;
        if !known || from != session {
            crate::plog!("agent: refused a client from another session");
            continue;
        }
        id += 1;
        tokio::spawn(serve_client(client, id));
    }
}

async fn serve_client(pipe: NamedPipeServer, id: u64) {
    CLIENTS.fetch_add(1, Ordering::Relaxed);
    let who = format!("[pipe {id}]");
    crate::plog!("{who} connected");
    let (mut rd, mut wr) = tokio::io::split(pipe);
    // Small on purpose, like the daemon's own bulk queue: a screen frame that finds it full is
    // skipped by the capture thread, never queued.
    let (tx, mut rx) = mpsc::channel::<Value>(16);
    let writer = tokio::spawn(async move {
        while let Some(value) = rx.recv().await {
            if proto::write_frame(&mut wr, &value).await.is_err() {
                break;
            }
        }
    });
    let hello = proto::map(vec![
        ("t", proto::s("agent.hello")),
        ("proto", Value::from(AGENT_PROTO)),
        ("version", proto::s(env!("CARGO_PKG_VERSION"))),
    ]);
    if tx.send(hello).await.is_ok() {
        let mut desk = LocalDesktop::new(who.clone());
        while let Ok(Some(frame)) = proto::read_frame(&mut rd).await {
            if let Some(err) = desk.handle(&frame, &tx) {
                let _ = tx.send(err).await;
            }
        }
    }
    drop(tx);
    let _ = writer.await;
    crate::plog!("{who} gone");
}
