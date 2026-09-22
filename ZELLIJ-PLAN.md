# Plan: Zellij as the Dev-plugin shell multiplexer

## Why

The phone renders the shell with a hand-rolled partial-xterm emulator
(`TerminalEmulator` / `AnsiParser`). Windows ConPTY output exercises sequences the
emulator doesn't fully implement, so complex programs and prompt themes render
wrong.

Running the shell inside **Zellij** (Rust, single binary, native Windows builds,
its own session persistence, tabs/panes, a `zellij action` CLI) re-renders
everything into a small, stable capability set. The emulator then only has to be
"Zellij-compatible", and panes/tabs come for free.

Alternative considered: keep no multiplexer, just harden the emulator and add
pane splitting to plaind's own session manager. More control, no runtime
dependency, but reinvents Zellij and is more work. Zellij is the faster path to
"the shell just looks right".

---

## Phase 0 — Emulator prerequisites (needed for *any* multiplexer)

Zellij queries the terminal on startup and will misbehave without answers.

1. **Answer capability queries** in `AnsiParser`, sending replies back up the
   `pty` channel as input bytes:
   - `ESC [ c` / `ESC [ > c` (DA1/DA2) → a VT220-ish response.
   - `ESC [ 6 n` (DSR) → `ESC [ <row> ; <col> R`.
   - `ESC [ 5 n` → `ESC [ 0 n`.
2. **Advertise the terminal**: plaind sets `TERM=xterm-256color` and
   `COLORTERM=truecolor` in the PTY environment so Zellij enables RGB.
3. **Streaming UTF-8 decoder** in `TerminalEmulator.feed` — buffer partial
   multi-byte sequences across `pty.data` chunks (Zellij's borders and status bar
   are heavily multi-byte).
4. **Box-drawing / block glyphs** render correctly (font-coverage check;
   Cascadia Mono covers them).
5. **DEC private modes** Zellij uses: `?1049` alt-screen, `?25` cursor show/hide,
   `?2004` bracketed paste. Swallow `?1000`–`?1006` mouse modes.
6. **Wide-character width table** (East-Asian width) so status-bar / plugin glyphs
   advance the right number of cells.

Milestone: a real `tmux` or `zellij` run under plaind renders on the phone
without garbling.

---

## Phase 1 — plaind: run the shell inside Zellij (opt-in)

1. **Detect a binary**: `$PLAIND_ZELLIJ` override → `zellij` on `PATH` →
   `wsl zellij` → none.
2. **Config**: `PLAIND_MUX = zellij | tmux | none` (default `none`), plus a
   per-session override field in `session.open`.
3. **Spawn**: when enabled, the PTY command becomes
   `zellij --layout <phone.kdl> attach --create <name>` instead of the bare
   shell. `<name>` = the plaind session id so plaind persistence and Zellij
   sessions line up.
4. **Ship `phone.kdl`**: one pane, compact chrome (minimal or hidden Zellij
   status/tab bar), default shell.
5. **Cleanup**: on `session.kill`, also `zellij delete-session <name>` so Zellij
   sessions don't leak.
6. **Resize**: forward `pty.resize` to the Zellij PTY unchanged; Zellij relayouts
   internally.
7. Report `mux: "zellij"` in `session.opened`.

---

## Phase 2 — Phone: Zellij-aware controls

1. Read `mux` from `session.opened`. When `"zellij"`, the shell key bar gains a
   mux row: **new pane, new tab, next/prev pane, next/prev tab, close pane,
   toggle fullscreen, detach**.
2. Prefer a **`mux.action` protocol message** over sending raw keybindings:
   phone → `{t:"mux.action", ch, verb:"new-pane"}` → plaind runs
   `zellij -s <name> action <verb>`. No keybinding coupling.
3. Optional tab strip in the phone UI mirroring Zellij tabs (parse
   `zellij action dump-layout`, or just show what Zellij draws).
4. Pinch-zoom / font size unchanged — Zellij content is plain text.
5. When muxed, **disable the phone's own scrollback overlay** and let Zellij own
   scroll / search / copy mode.

---

## Phase 3 — Polish

1. Phone-tuned Zellij config: decide mouse-mode capture (off, or map touch →
   pane focus).
2. Layout presets per device (e.g. "editor + terminal" split).
3. **Bundle the `zellij` binary** with the plaind Windows build (or a one-line
   install prompt) for zero setup.
4. Tray / settings toggle for `PLAIND_MUX`.
5. Docs.

---

## Testing

- ConPTY: run `zellij` directly under plaind on native Windows — verify borders,
  status bar, colours render on the phone. If broken, fall back to the WSL path
  and document.
- `vim` / `htop` / `less` inside a Zellij pane — the original goal.
- Detach / reattach: kill the phone app, reopen → session + panes intact.
- Resize: rotate / pinch-zoom → Zellij relayouts cleanly.
- Multi-device: a Zellij session per host, no cross-talk.

---

## Open decisions

- Bundle the `zellij` binary, require an install, or WSL-only first?
- `mux.action` protocol vs raw keybindings?
- Does native-Windows ConPTY + Zellij work well enough, or target
  `tmux`-in-MSYS/WSL first?
- Keep the phone's scrollback/search, or fully delegate to Zellij when muxed?
