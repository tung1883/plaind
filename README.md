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
| `src/procs.rs` | the `proc` channel (`sysinfo`) |
| `tests/smoke.rs` | end-to-end handshake + shell round-trip |
