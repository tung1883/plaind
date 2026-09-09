# Dev-plugin wire protocol

`PROTO = 2`. Mirrored in the Plain app's `DevProtocol.java` and vendored at
`docs/dev-plugin-protocol.md`.

## Framing

- Plain TCP. Daemon listens on `0.0.0.0:8471` (`$PLAIND_PORT` to override).
- Frame = `uint32` big-endian length + a MessagePack **map**. Max 1 MiB.
- Kinds used: nil, bool, int, float64, string, binary, array, map.

## Handshake

| dir | message |
|---|---|
| C→D | `{t:"hello", proto:2, token, device}` |
| D→C | `{t:"welcome", proto:2, host, os:"linux\|macos\|windows", caps}` |
| D→C | `{t:"error", code:"auth", msg}` then close — bad token |

- `caps` ⊆ `["pty","session","proc","screen","input"]` (`screen`/`input` are build-time).
- Client pings `{t:"ping"}` every 15 s → `{t:"pong"}`. 20 s silence = dead.

## Channels

Stream messages carry a client-assigned integer `ch`. `input.*` is
connection-global (no `ch`).

### shell sessions

A session is a pty + shell that lives in the daemon, independent of any
connection. It keeps running after the client detaches; the daemon buffers the
last 256 KB of output and replays it on reattach.

| dir | message |
|---|---|
| C→D | `{t:"session.list", ch}` |
| D→C | `{t:"session.list", ch, sessions:[{id, name, cols, rows, alive, created_ms}]}` |
| C→D | `{t:"session.open", ch, id?, name?, cols, rows}` — `id` given = reattach; else create |
| D→C | `{t:"session.opened", ch, id, name, cols, rows, alive}` then a `pty.data` replay |
| D→C | `{t:"session.gone", ch, id}` — reply to `session.open {id}` for an id the daemon no longer has (restarted / killed); nothing is created |
| C→D | `{t:"session.detach", ch}` — unbind; the shell keeps running |
| C→D | `{t:"session.kill", ch, id}` — terminate the shell |
| C↔D | `{t:"pty.data", ch, data:<bin>}` — output / keystrokes for the bound session |
| C→D | `{t:"pty.resize", ch, cols, rows}` |
| D→C | `{t:"pty.exit", ch, code}` |

`{t:"pty.open", ch, cols, rows, cmd:null|"<str>"}` still works: it creates an
**ephemeral** session that is killed when its channel closes (used by the test
rig). `pty.close` detaches, and kills if the session is ephemeral.

Durability: sessions survive a client disconnect, not a daemon restart. On Unix,
set `PLAIND_TMUX=1` and each shell launches inside `tmux new -A -s plain_<name>`,
which survives restarts too.

### screen

| dir | message |
|---|---|
| C→D | `{t:"screen.start", ch, max_w, fps, cursor?:bool}` |
| C→D | `{t:"screen.stop", ch}` |
| D→C | `{t:"screen.frame", ch, w, h, sw, sh, format:"jpeg", full:true, data:<bin>}` |

- Whole-frame JPEG at `fps` (1–60; capture + encode may cap the real rate lower).
  `w`,`h` = delivered size; `sw`,`sh` =
  source monitor size (for client-side cursor scaling).
- `cursor` (default true) = daemon draws the pointer on each frame; phone
  sends `false` and draws its own.
- Re-send `screen.start` on the same `ch` to change `max_w`/`fps` live
  (e.g. raise `max_w` when zoomed); the daemon swaps the stream.

### input

| message |
|---|
| `{t:"input.move", dx, dy, scroll?}` — relative move / scroll |
| `{t:"input.zoom", ticks}` — signed wheel ticks with Ctrl held (trackpad pinch) |
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
| D→C | `{t:"proc.list", ch, procs:[{pid, name, cpu, mem_kb, state:"R\|S\|T\|Z\|?", user}], sys}` — procs cpu desc, ≤300; per-proc `cpu` is 0–100 of the whole machine (already ÷ cores) |
| C→D | `{t:"proc.kill", ch, pid, sig:"TERM\|KILL"}` |
| D→C | `{t:"proc.killed", ch, pid, ok}` |

`sys` = `{cpu (0–100), cpu_count, mem_used_kb, mem_total_kb, swap_used_kb,
swap_total_kb, load:[1m,5m,15m], uptime_s,
tasks:{total,running,sleeping,stopped,zombie,other}}` (`load` is zeros on Windows).

## Errors

`{t:"error", code, msg}` — closes the socket, or scoped to a channel if it
carries `ch`.
