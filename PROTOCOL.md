# Dev-plugin wire protocol

`PROTO = 1`. Mirrored in the Plain app's `DevProtocol.java` and vendored at
`docs/dev-plugin-protocol.md`.

## Framing

- Plain TCP. Daemon listens on `0.0.0.0:8471` (`$PLAIND_PORT` to override).
- Frame = `uint32` big-endian length + a MessagePack **map**. Max 1 MiB.
- Kinds used: nil, bool, int, float64, string, binary, array, map.

## Handshake

| dir | message |
|---|---|
| C→D | `{t:"hello", proto:1, token, device}` |
| D→C | `{t:"welcome", proto:1, host, os:"linux\|macos\|windows", caps}` |
| D→C | `{t:"error", code:"auth", msg}` then close — bad token |

- `caps` ⊆ `["pty","proc","screen","input"]` (`screen`/`input` are build-time).
- Client pings `{t:"ping"}` every 15 s → `{t:"pong"}`. 20 s silence = dead.

## Channels

Stream messages carry a client-assigned integer `ch`. `input.*` is
connection-global (no `ch`).

### pty

| dir | message |
|---|---|
| C→D | `{t:"pty.open", ch, cols, rows, cmd:null\|"<str>"}` — null = login shell |
| C↔D | `{t:"pty.data", ch, data:<bin>}` |
| C→D | `{t:"pty.resize", ch, cols, rows}` |
| C→D | `{t:"pty.close", ch}` |
| D→C | `{t:"pty.exit", ch, code}` |

### screen

| dir | message |
|---|---|
| C→D | `{t:"screen.start", ch, max_w, fps, cursor?:bool}` |
| C→D | `{t:"screen.stop", ch}` |
| D→C | `{t:"screen.frame", ch, w, h, sw, sh, format:"jpeg", full:true, data:<bin>}` |

- Whole-frame JPEG at `fps` (1–30). `w`,`h` = delivered size; `sw`,`sh` =
  source monitor size (for client-side cursor scaling).
- `cursor` (default true) = daemon draws the pointer on each frame; phone
  sends `false` and draws its own.
- Re-send `screen.start` on the same `ch` to change `max_w`/`fps` live
  (e.g. raise `max_w` when zoomed); the daemon swaps the stream.

### input

| message |
|---|
| `{t:"input.move", dx, dy, scroll?}` — relative move / scroll |
| `{t:"input.point", x, y}` — `x`,`y` ∈ 0..1 of the full monitor (absolute) |
| `{t:"input.click", button:"l\|r\|m", double?:true}` |
| `{t:"input.down"}` / `{t:"input.up"}` — press-drag |
| `{t:"input.key", text?, key?, mods?:["ctrl"\|"alt"\|"shift"\|"meta"]}` |

`key` ∈ `Enter Backspace Delete Tab Escape Up Down Left Right Home End
PageUp PageDown Space ctrl-alt-delete`. `mods` are held around `text`/`key`.

### proc

| dir | message |
|---|---|
| C→D | `{t:"proc.list", ch}` |
| D→C | `{t:"proc.list", ch, procs:[{pid, name, cpu, mem_kb, user}]}` — cpu desc, ≤300 |
| C→D | `{t:"proc.kill", ch, pid, sig:"TERM\|KILL"}` |
| D→C | `{t:"proc.killed", ch, pid, ok}` |

## Errors

`{t:"error", code, msg}` — closes the socket, or scoped to a channel if it
carries `ch`.
