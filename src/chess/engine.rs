//! A UCI engine child process (Stockfish) behind the [`Analyzer`] trait. Blocking API —
//! each worker thread owns one. A reader thread forwards stdout lines over a channel so a
//! hung or crashed engine shows up as a timeout / disconnect instead of a stuck thread.

use anyhow::{anyhow, bail, Result};
use shakmaty::{fen::Fen, Chess, EnPassantMode};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use super::score::Score;

/// One engine line: score (side-to-move POV), reached depth and principal variation in UCI.
#[derive(Clone, Debug)]
pub struct Analysis {
    pub score: Score,
    pub depth: u32,
    pub pv: Vec<String>,
}

/// What the puzzle generator needs from an engine. `movetime_ms == 0` means no time cap.
/// Returns the lines best-first; empty for a terminal position.
pub trait Analyzer {
    fn analyze(&mut self, pos: &Chess, depth: u32, lines: u32, movetime_ms: u32) -> Result<Vec<Analysis>>;
    /// Called at the start of each game (clears engine-side state).
    fn new_game(&mut self) -> Result<()> { Ok(()) }
}

impl Analyzer for Box<dyn Analyzer + Send> {
    fn analyze(&mut self, pos: &Chess, depth: u32, lines: u32, movetime_ms: u32) -> Result<Vec<Analysis>> {
        (**self).analyze(pos, depth, lines, movetime_ms)
    }
    fn new_game(&mut self) -> Result<()> { (**self).new_game() }
}

#[derive(Clone, Copy, Debug)]
pub struct EngineOpts {
    pub threads: u32,
    pub hash_mb: u32,
}

pub struct Engine {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    last_multipv: u32,
}

fn command(path: &Path) -> Command {
    let mut cmd = Command::new(path);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

impl Engine {
    pub fn spawn(path: &Path, opts: EngineOpts) -> Result<Engine> {
        let mut child = command(path)
            .spawn()
            .map_err(|e| anyhow!("cannot start engine {}: {e}", path.display()))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no engine stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no engine stdout"))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let mut e = Engine { child, stdin, rx, last_multipv: 1 };
        e.send("uci")?;
        e.wait_for("uciok", Duration::from_secs(20))?;
        e.send(&format!("setoption name Threads value {}", opts.threads.max(1)))?;
        e.send(&format!("setoption name Hash value {}", opts.hash_mb.max(1)))?;
        e.ready()?;
        Ok(e)
    }

    fn send(&mut self, cmd: &str) -> Result<()> {
        self.stdin.write_all(cmd.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn wait_for(&mut self, token: &str, timeout: Duration) -> Result<()> {
        let end = Instant::now() + timeout;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(l) if l.trim() == token => return Ok(()),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => bail!("engine timed out waiting for {token}"),
                Err(RecvTimeoutError::Disconnected) => bail!("engine exited before {token}"),
            }
        }
    }

    fn ready(&mut self) -> Result<()> {
        self.send("isready")?;
        self.wait_for("readyok", Duration::from_secs(30))
    }

    /// Engine name from the `id name` response, e.g. "Stockfish 17.1". None if it won't run or
    /// doesn't answer within 10 s (a wedged binary must not hang the caller).
    pub fn id_name(path: &Path) -> Option<String> {
        let mut child = command(path).spawn().ok()?;
        let mut stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        stdin.write_all(b"uci
").ok()?;
        stdin.flush().ok()?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut name = None;
            for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
                if let Some(n) = line.strip_prefix("id name ") {
                    name = Some(n.trim().to_string());
                }
                if line.trim() == "uciok" {
                    break;
                }
            }
            let _ = tx.send(name);
        });
        let name = rx.recv_timeout(Duration::from_secs(10)).ok().flatten();
        let _ = stdin.write_all(b"quit
");
        let _ = child.kill(); // also unblocks the reader thread
        let _ = child.wait();
        name
    }
}

impl Analyzer for Engine {
    fn analyze(&mut self, pos: &Chess, depth: u32, lines: u32, movetime_ms: u32) -> Result<Vec<Analysis>> {
        let lines = lines.max(1);
        if lines != self.last_multipv {
            self.send(&format!("setoption name MultiPV value {lines}"))?;
            self.last_multipv = lines;
        }
        let fen = Fen::from_position(pos, EnPassantMode::Legal);
        self.send(&format!("position fen {fen}"))?;
        let mut go = format!("go depth {depth}");
        if movetime_ms > 0 {
            go.push_str(&format!(" movetime {movetime_ms}"));
        }
        self.send(&go)?;

        // Generous hard ceiling so a wedged engine can't hold a worker forever.
        let budget = if movetime_ms > 0 { movetime_ms as u64 + 15_000 } else { 15 * 60_000 };
        let end = Instant::now() + Duration::from_millis(budget);
        let mut slots: Vec<Option<Analysis>> = vec![None; lines as usize + 1];
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let line = match self.rx.recv_timeout(left) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => {
                    let _ = self.send("stop");
                    bail!("engine search timed out");
                }
                Err(RecvTimeoutError::Disconnected) => bail!("engine exited mid-search"),
            };
            if line.starts_with("bestmove") {
                break;
            }
            if let Some((mp, a)) = parse_info(&line) {
                if mp >= 1 && (mp as usize) < slots.len() {
                    slots[mp as usize] = Some(a);
                }
            }
        }
        Ok(slots.into_iter().flatten().collect())
    }

    fn new_game(&mut self) -> Result<()> {
        self.send("ucinewgame")?;
        self.ready()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.send("quit");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Parses an `info … multipv N … score cp|mate X … pv …` line; None for anything without a pv.
pub fn parse_info(line: &str) -> Option<(u32, Analysis)> {
    if !line.starts_with("info") || !line.contains(" pv ") {
        return None;
    }
    let t: Vec<&str> = line.split_whitespace().collect();
    let (mut mp, mut depth) = (1u32, 0u32);
    let mut score = None;
    let mut pv = Vec::new();
    let mut i = 0;
    while i < t.len() {
        match t[i] {
            "multipv" if i + 1 < t.len() => mp = t[i + 1].parse().ok()?,
            "depth" if i + 1 < t.len() => depth = t[i + 1].parse().ok()?,
            "score" if i + 2 < t.len() => {
                score = match t[i + 1] {
                    "cp" => Some(Score::Cp(t[i + 2].parse().ok()?)),
                    "mate" => Some(Score::Mate(t[i + 2].parse().ok()?)),
                    _ => score,
                }
            }
            "pv" => {
                pv = t[i + 1..].iter().map(|s| s.to_string()).collect();
                break;
            }
            _ => {}
        }
        i += 1;
    }
    if pv.is_empty() {
        return None;
    }
    Some((mp, Analysis { score: score?, depth, pv }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cp_line() {
        let (mp, a) = parse_info("info depth 12 seldepth 18 multipv 2 score cp -34 nodes 1 nps 2 time 3 pv e2e4 e7e5 g1f3").unwrap();
        assert_eq!(mp, 2);
        assert_eq!(a.depth, 12);
        assert_eq!(a.score, Score::Cp(-34));
        assert_eq!(a.pv, vec!["e2e4", "e7e5", "g1f3"]);
    }

    #[test]
    fn parses_mate_line_with_bound() {
        let (mp, a) = parse_info("info depth 5 score mate 3 lowerbound pv d1h5").unwrap();
        assert_eq!(mp, 1);
        assert_eq!(a.score, Score::Mate(3));
    }

    #[test]
    fn ignores_non_pv_lines() {
        assert!(parse_info("info depth 5 currmove e2e4 currmovenumber 1").is_none());
        assert!(parse_info("bestmove e2e4").is_none());
        assert!(parse_info("info string NNUE evaluation").is_none());
    }
}
