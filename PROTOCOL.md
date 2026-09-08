# Dev-plugin wire protocol

Canonical spec. The phone app mirrors this in `DevProtocol.java`; a copy lives in
the Plain repo at `docs/dev-plugin-protocol.md`. Current version: **PROTO = 1**.

## Transport & framing

Plain TCP. The daemon listens on `0.0.0.0:8471` (override with `$PLAIND_PORT`).

Every frame: `uint32` big-endian length, then that many bytes of a MessagePack
**map**. Maximum frame 1 MiB. Only these MessagePack kinds appear: nil, bool,
integer, float64, string, binary, array, map.

## Handshake

1. Client → `{t:"hello", proto:1, token:"<pairing-token>", device:"<name>"}`
2. Daemon checks the token against its paired list.
   - ok → `{t:"welcome", proto:1, host:"<hostname>", os:"linux|macos|windows", caps:[…]}`
   - bad → `{t:"error", code:"auth", msg:"…"}` then close
   `caps` is a subset of `["pty","proc","screen","input"]` — the last two depend
   on how the daemon was built.
3. Client sends `{t:"ping"}` every 15 s; daemon replies `{t:"pong"}`. 20 s of
   silence = dead link.

## Channels

A message that operates on a stream carries a `ch` (integer, client-assigned).
`input.*` has no `ch` — it is connection-global.

### pty

| dir | message |
|---|---|
| C→D | `{t:"pty.open", ch, cols, rows, cmd:null|"<string>"}` — `cmd` null = login shell |
| D→C | `{t:"pty.data", ch, data:<bin>}` |
| C→D | `{t:"pty.data", ch, data:<bin>}` |
| C→D | `{t:"pty.resize", ch, cols, rows}` |
| D→C | `{t:"pty.exit", ch, code}` |
| C→D | `{t:"pty.close", ch}` |

### screen

| dir | message |
|---|---|
| C→D | `{t:"screen.start", ch, max_w, fps}` |
| D→C | `{t:"screen.frame", ch, w, h, format:"jpeg", full:true, data:<bin>}` |
| C→D | `{t:"screen.stop", ch}` |

Whole-frame JPEG at `fps` (clamped 1–30). Tiled / delta encoding is a later
optimisation and would arrive as a new `format`.

### input

| message |
|---|
| `{t:"input.move", dx, dy, scroll?}` |
| `{t:"input.click", button:"l|r|m", double?:true}` |
| `{t:"input.down"}` / `{t:"input.up"}` — press-drag |
| `{t:"input.key", text?:"<utf8>", key?:"Enter|Backspace|Tab|Escape|Up|Down|Left|Right|Space|ctrl-alt-delete"}` |

### proc

| dir | message |
|---|---|
| C→D | `{t:"proc.list", ch}` |
| D→C | `{t:"proc.list", ch, procs:[{pid, name, cpu, mem_kb, user}]}` (sorted by cpu desc, capped at 300) |
| C→D | `{t:"proc.kill", ch, pid, sig:"TERM|KILL"}` |
| D→C | `{t:"proc.killed", ch, pid, ok}` |

## Errors

`{t:"error", code, msg}` — connection-level (closes the socket) or, with a `ch`,
scoped to that channel (e.g. `screen.start` on a headless build).
