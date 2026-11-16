<h1><img src="assets/plaind-128.png" alt="plaind icon" width="40" align="left" /> plaind</h1>

Companion daemon for the **Plain** phone's Dev plugin. One TCP socket carries an
interactive shell, a low-fi screen mirror and a process list to the phone.

```
plaind pair     # mint a pairing token, print its plaind:// link + QR
plaind          # run the daemon (port 8471, or $PLAIND_PORT)
```

Pair once: run `plaind pair` on the computer, scan or paste the link into
**Dev → Add computer** on the phone. The token is trusted from then on
(`~/.config/plaind/paired.txt`, `%APPDATA%\plaind\paired.txt` on Windows).

The transport is plain TCP — bind it to a trusted network (a Tailscale
interface, a LAN behind a firewall). Mutual-auth encryption (Noise) is planned;
today the pairing token is the only credential and it is sent in clear.

## Build

```
cargo build --release
```

Needs a C toolchain for the native deps (PTY, screen capture, input). On Windows
either MSVC Build Tools or the GNU toolchain
(`cargo +stable-x86_64-pc-windows-gnu build`).

Screen capture and input injection are the default features `screen` and
`input`; a headless box can drop them:

```
cargo build --release --no-default-features
```

## Full control on Windows

plaind is an ordinary program, and Windows keeps an ordinary program away from
some of the screen: while an Administrator window has focus its mouse and
keyboard input is dropped, and it can neither see nor answer a UAC prompt or
the lock screen. An optional service lifts that:

```
plaind service install     # from an Administrator terminal, or the tray's "Install full control…"
plaind service status
plaind service uninstall
```

`install` copies plaind to `C:\Program Files\plaind\` and registers a service
that runs as SYSTEM. The service keeps a small **desktop agent** running in the
session that owns the screen; the daemon hands that agent the phone's `screen`
and `input` frames over a local pipe and relays its frames back. Shells, file
sync and the clipboard stay in the daemon, running as you. Nothing changes on
the phone. The tray shows `desktop: full control` while an agent is in use.
Without the service plaind works as before.

After rebuilding, run `plaind service install` again to update the copy. Logs:
`C:\ProgramData\plaind\service.log` and `agent.log`; a failed install started
from the tray leaves its reason in `last-error.txt` there.

Things to know:

- Any program running under your Windows account can use the agent's pipe, and
  so can make it click, including on a UAC prompt. The pipe is closed to other
  accounts, but not to your own programs.
- `install` lets services send Ctrl+Alt+Del (the `SoftwareSASGeneration` policy);
  `uninstall` puts the policy back.
- A sleeping computer still cannot be reached, and neither can the sign-in
  screen after a reboot, before anyone has logged in.

`cargo run --example inputprobe` tells whether input sent through a running
plaind reaches Windows with whatever window currently has focus.

## Protocol

See [PROTOCOL.md](PROTOCOL.md). Bump `PROTO` on any breaking change — the phone
rejects a mismatched major during the handshake.

## Layout

| file | role |
|---|---|
| `src/proto.rs` | framing + MessagePack helpers |
| `src/pairing.rs` | token store, `pair` subcommand |
| `src/session.rs` | one connection: handshake + frame loop |
| `src/pty.rs` | the `pty` channel (a real shell) |
| `src/screen.rs` | the `screen` channel (JPEG frames) |
| `src/input.rs` | the `input` channel (mouse + keyboard) |
| `src/desktop.rs` | routes `screen` / `input` to this process or to the desktop agent |
| `src/service.rs` | Windows: the SYSTEM service (`plaind service …`) |
| `src/agent.rs` | Windows: the desktop agent the service runs |
| `src/windesk.rs` | Windows: session, token and desktop helpers |
| `src/procs.rs` | the `proc` channel (`sysinfo`) |
| `tests/smoke.rs` | end-to-end handshake + shell round-trip |
