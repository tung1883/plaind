//! Puzzle-generation jobs. A job lives here, in the process-global [`registry`], independent of
//! any phone connection: it keeps running when the phone locks, sleeps or disconnects, and the
//! phone later re-attaches and is replayed every result it hasn't acknowledged. Like the shell
//! sessions in `pty.rs`, the registry — not the connection — owns the work.
//!
//! Delivery model: every finished game appends one [`ResultItem`] (with a sequence number, and
//! the puzzle if one was found) to the job's log. The phone acks `upto` a sequence number; the
//! log is pruned to what's unacked. The same log is persisted (`results.jsonl`), as are the
//! received games (`games.jsonl`) and the job's meta (`job.json`), so a daemon restart resumes
//! where it stopped.

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Instant;
use tokio::sync::Notify;

use super::engine::{Analyzer, EngineOpts};
use super::generator::{Game, GenParams, Generator, Puzzle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Done,
    Cancelled,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Done => "done",
            State::Cancelled => "cancelled",
            State::Failed => "failed",
        }
    }
    fn parse(s: &str) -> State {
        match s {
            "done" => State::Done,
            "cancelled" => State::Cancelled,
            "failed" => State::Failed,
            _ => State::Running,
        }
    }
    pub fn is_terminal(self) -> bool { self != State::Running }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResultItem {
    pub seq: u64,
    /// Id of the game this result is for (the phone records it as scanned).
    pub game: String,
    pub puzzle: Option<Puzzle>,
}

/// How a job is run (everything but the generator thresholds).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCfg {
    pub workers: u32,
    pub threads: u32,
    pub hash_mb: u32,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    id: String,
    params: GenParams,
    cfg: RunCfg,
    total: u32,
    games_complete: bool,
    state: String,
    error: Option<String>,
}

/// Builds one engine for a worker. Production: spawn Stockfish. Tests: a scripted fake.
pub type EngineFactory = Arc<dyn Fn(EngineOpts) -> Result<Box<dyn Analyzer + Send>> + Send + Sync>;

struct Inner {
    state: State,
    error: Option<String>,
    total: u32,
    known: HashSet<String>,
    queue: VecDeque<Game>,
    games_complete: bool,
    scanned: u32,
    found: u32,
    errors: u32,
    results: Vec<ResultItem>,
    next_seq: u64,
    acked: u64,
    active_workers: u32,
    /// Games finished before this run of the process started (restored), for the ETA rate.
    scanned_at_start: u32,
}

pub struct Job {
    pub id: String,
    pub params: GenParams,
    pub cfg: RunCfg,
    dir: PathBuf,
    inner: Mutex<Inner>,
    cv: Condvar,
    cancel: AtomicBool,
    /// Wakes attached pumps whenever something worth sending changed.
    pub notify: Notify,
    started: Instant,
}

/// A point-in-time view for the wire.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: String,
    pub state: State,
    pub error: Option<String>,
    pub total: u32,
    pub have: u32,
    pub games_complete: bool,
    pub scanned: u32,
    pub found: u32,
    pub errors: u32,
    pub workers: u32,
    pub last_seq: u64,
    pub acked: u64,
    pub elapsed_s: u64,
    pub eta_s: Option<u64>,
    pub cfg: RunCfg,
}

impl Job {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn snapshot(&self) -> Snapshot {
        let g = self.lock();
        let elapsed = self.started.elapsed().as_secs();
        let done_here = g.scanned.saturating_sub(g.scanned_at_start);
        let eta_s = if g.state == State::Running && done_here > 0 && g.total > g.scanned {
            let rate = done_here as f64 / self.started.elapsed().as_secs_f64().max(1.0);
            Some(((g.total - g.scanned) as f64 / rate) as u64)
        } else {
            None
        };
        Snapshot {
            id: self.id.clone(),
            state: g.state,
            error: g.error.clone(),
            total: g.total,
            have: g.known.len() as u32,
            games_complete: g.games_complete,
            scanned: g.scanned,
            found: g.found,
            errors: g.errors,
            workers: g.active_workers,
            last_seq: g.next_seq,
            acked: g.acked,
            elapsed_s: elapsed,
            eta_s,
            cfg: self.cfg,
        }
    }

    /// Up to `max` results with `seq > after`, oldest first.
    pub fn results_after(&self, after: u64, max: usize) -> Vec<ResultItem> {
        let g = self.lock();
        g.results.iter().filter(|r| r.seq > after).take(max).cloned().collect()
    }

    /// The phone has stored everything up to `upto`: drop it from memory (it stays on disk
    /// until the job is removed, but is never replayed — see `acked`).
    pub fn ack(&self, upto: u64) {
        let mut g = self.lock();
        if upto > g.acked {
            g.acked = upto.min(g.next_seq);
            let acked = g.acked;
            g.results.retain(|r| r.seq > acked);
            let _ = std::fs::write(self.dir.join("acked"), acked.to_string());
        }
    }

    /// Adds games the job hasn't seen (by id). Returns how many games it now holds. The games
    /// are on disk before they can be picked up, so a restart never loses accepted games.
    pub fn add_games(&self, games: Vec<Game>) -> u32 {
        let mut g = self.lock();
        if g.state.is_terminal() {
            return g.known.len() as u32;
        }
        let mut any = false;
        let mut failure = None;
        for game in games {
            if g.known.contains(&game.id) {
                continue;
            }
            if let Err(e) = append_line(&self.dir.join("games.jsonl"), &game) {
                failure = Some(format!("disk: cannot save games: {e}"));
                break;
            }
            g.known.insert(game.id.clone());
            g.queue.push_back(game);
            any = true;
        }
        if let Some(msg) = failure {
            self.fail_locked(&mut g, msg);
            let have = g.known.len() as u32;
            drop(g);
            self.after_fail();
            return have;
        }
        let have = g.known.len() as u32;
        drop(g);
        if any {
            self.cv.notify_all();
            self.notify.notify_waiters();
        }
        have
    }

    /// No more games are coming: workers exit once the queue drains.
    pub fn end_games(&self) {
        {
            let mut g = self.lock();
            g.games_complete = true;
            self.write_meta(&g);
        }
        self.cv.notify_all();
        self.notify.notify_waiters();
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        {
            let mut g = self.lock();
            g.queue.clear();
        }
        self.cv.notify_all();
        self.notify.notify_waiters();
        // With no worker running (e.g. waiting on a missing engine), finalise right here.
        let idle = self.lock().active_workers == 0;
        if idle {
            self.finalize();
        }
    }

    fn cancelled(&self) -> bool { self.cancel.load(Ordering::Relaxed) }

    fn fail(&self, msg: String) {
        {
            let mut g = self.lock();
            self.fail_locked(&mut g, msg);
        }
        self.after_fail();
    }

    /// Marks the job failed (first failure wins) and persists it. Caller holds the lock.
    fn fail_locked(&self, g: &mut Inner, msg: String) {
        if g.state.is_terminal() {
            return;
        }
        g.state = State::Failed;
        g.error = Some(msg);
        g.queue.clear();
        self.write_meta(g);
    }

    /// Stop the other workers and wake everyone, after a failure. Caller must NOT hold the lock.
    fn after_fail(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cv.notify_all();
        self.notify.notify_waiters();
    }

    /// Next game to analyze; blocks while the phone is still uploading; None = finished/cancelled.
    fn next_game(&self) -> Option<Game> {
        let mut g = self.lock();
        loop {
            if self.cancelled() {
                return None;
            }
            if let Some(game) = g.queue.pop_front() {
                return Some(game);
            }
            if g.games_complete {
                return None;
            }
            g = self.cv.wait(g).unwrap_or_else(|p| p.into_inner());
        }
    }

    fn complete(&self, game_id: &str, puzzle: Option<Puzzle>, errored: bool) {
        let failed;
        {
            let mut g = self.lock();
            let item = ResultItem { seq: g.next_seq + 1, game: game_id.to_string(), puzzle };
            // Disk first, then memory: anything the phone can see is already durable. If the
            // disk refuses, the job fails loudly instead of promising durability it lacks.
            match append_line(&self.dir.join("results.jsonl"), &item) {
                Ok(()) => {
                    g.next_seq = item.seq;
                    g.scanned += 1;
                    if item.puzzle.is_some() {
                        g.found += 1;
                    }
                    if errored {
                        g.errors += 1;
                    }
                    g.results.push(item);
                    failed = false;
                }
                Err(e) => {
                    self.fail_locked(&mut g, format!("disk: cannot save results: {e}"));
                    failed = true;
                }
            }
        }
        if failed {
            self.after_fail();
        }
        self.notify.notify_waiters();
    }

    /// Called by each worker as it exits; the last one settles the final state.
    fn worker_exit(&self) {
        let last = {
            let mut g = self.lock();
            g.active_workers = g.active_workers.saturating_sub(1);
            g.active_workers == 0
        };
        if last {
            self.finalize();
        } else {
            self.notify.notify_waiters();
        }
    }

    fn finalize(&self) {
        {
            let mut g = self.lock();
            if g.state == State::Running {
                g.state = if self.cancelled() {
                    State::Cancelled
                } else if g.games_complete && g.queue.is_empty() {
                    State::Done
                } else {
                    g.error = Some("workers stopped before all games were analyzed".into());
                    State::Failed
                };
                self.write_meta(&g);
            }
        }
        self.notify.notify_waiters();
    }

    fn persist_meta(&self) {
        let g = self.lock();
        self.write_meta(&g);
    }

    /// Writes job.json for the state in `g`. Callers that change `state` do so and write while
    /// holding the lock, so nobody can observe a terminal state that isn't on disk yet.
    fn write_meta(&self, g: &Inner) {
        let meta = Meta {
            id: self.id.clone(),
            params: self.params.clone(),
            cfg: self.cfg,
            total: g.total,
            games_complete: g.games_complete,
            state: g.state.as_str().to_string(),
            error: g.error.clone(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&meta) {
            let tmp = self.dir.join("job.json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, self.dir.join("job.json"));
            }
        }
    }
}

fn worker_main(job: Arc<Job>, factory: EngineFactory) {
    let opts = EngineOpts { threads: job.cfg.threads, hash_mb: job.cfg.hash_mb };
    let mut engine: Option<Box<dyn Analyzer + Send>> = None;
    let mut spawn_failures = 0;
    'games: while let Some(game) = job.next_game() {
        let mut attempts = 0;
        let outcome = loop {
            if engine.is_none() {
                match factory(opts) {
                    Ok(e) => {
                        engine = Some(e);
                        spawn_failures = 0;
                    }
                    Err(err) => {
                        spawn_failures += 1;
                        crate::plog!("[chess {}] engine start failed ({spawn_failures}/3): {err}", job.id);
                        if spawn_failures >= 3 {
                            job.fail(format!("engine: {err}"));
                            break 'games;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        continue;
                    }
                }
            }
            let eng = engine.as_mut().unwrap();
            let mut generator = Generator::new(eng, job.params.clone(), &job.cancel);
            match generator.analyze_game(&game) {
                Ok(p) => break Ok(p),
                Err(e) => {
                    engine = None; // respawn: it crashed or wedged
                    attempts += 1;
                    if attempts >= 2 {
                        break Err(e);
                    }
                }
            }
        };
        if job.cancelled() {
            break; // a half-analysed game is neither recorded nor scanned
        }
        match outcome {
            Ok(p) => job.complete(&game.id, p, false),
            Err(e) => {
                crate::plog!("[chess {}] game {} skipped after engine errors: {e}", job.id, game.id);
                job.complete(&game.id, None, true);
            }
        }
    }
    job.worker_exit();
}

fn spawn_workers(job: &Arc<Job>, factory: &EngineFactory) {
    let n = job.cfg.workers.max(1);
    job.lock().active_workers = n;
    for i in 0..n {
        let (j, f) = (job.clone(), factory.clone());
        std::thread::Builder::new()
            .name(format!("chess-{}-{i}", &job.id[..job.id.len().min(6)]))
            .spawn(move || worker_main(j, f))
            .expect("spawn chess worker");
    }
}

// --- registry ---------------------------------------------------------------------------

pub struct Registry {
    root: PathBuf,
    factory: EngineFactory,
    jobs: Mutex<Vec<Arc<Job>>>,
}

impl Registry {
    pub fn new(root: PathBuf, factory: EngineFactory) -> Registry {
        let _ = std::fs::create_dir_all(&root);
        Registry { root, factory, jobs: Mutex::new(Vec::new()) }
    }

    fn jobs(&self) -> MutexGuard<'_, Vec<Arc<Job>>> {
        self.jobs.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs().iter().find(|j| j.id == id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<Job>> { self.jobs().clone() }

    /// The one job that is still running, if any.
    pub fn active(&self) -> Option<Arc<Job>> {
        self.jobs().iter().find(|j| !j.lock().state.is_terminal()).cloned()
    }

    /// Creates a job and starts its workers (idle until games arrive). Idempotent on `id`.
    /// One running job at a time: a second `start` with a different id is refused.
    pub fn start(&self, id: &str, params: GenParams, cfg: RunCfg, total: u32) -> Result<Arc<Job>> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("bad job id");
        }
        if let Some(j) = self.get(id) {
            return Ok(j);
        }
        if let Some(a) = self.active() {
            bail!("busy: job {} is still running", a.id);
        }
        let dir = self.root.join(id);
        std::fs::create_dir_all(&dir)?;
        let job = Arc::new(Job {
            id: id.to_string(),
            params,
            cfg,
            dir,
            inner: Mutex::new(Inner {
                state: State::Running,
                error: None,
                total,
                known: HashSet::new(),
                queue: VecDeque::new(),
                games_complete: false,
                scanned: 0,
                found: 0,
                errors: 0,
                results: Vec::new(),
                next_seq: 0,
                acked: 0,
                active_workers: 0,
                scanned_at_start: 0,
            }),
            cv: Condvar::new(),
            cancel: AtomicBool::new(false),
            notify: Notify::new(),
            started: Instant::now(),
        });
        job.persist_meta();
        self.jobs().push(job.clone());
        spawn_workers(&job, &self.factory);
        Ok(job)
    }

    /// Deletes a finished job and its files. Refuses a running one (cancel it first).
    pub fn remove(&self, id: &str) -> Result<()> {
        let job = self.get(id).ok_or_else(|| anyhow!("no such job"))?;
        if !job.lock().state.is_terminal() {
            bail!("job is still running");
        }
        self.jobs().retain(|j| j.id != id);
        let _ = std::fs::remove_dir_all(&job.dir);
        Ok(())
    }

    /// Reloads jobs from disk after a daemon restart; unfinished ones resume. Jobs finished
    /// more than `keep_days` ago are deleted.
    pub fn restore(&self, keep_days: u64) {
        let Ok(rd) = std::fs::read_dir(&self.root) else { return };
        for e in rd.flatten() {
            let dir = e.path();
            if !dir.is_dir() {
                continue;
            }
            match self.load_job(&dir) {
                Ok(Some(job)) => {
                    let terminal = job.lock().state.is_terminal();
                    if terminal && age_days(&dir) > keep_days {
                        let _ = std::fs::remove_dir_all(&dir);
                        continue;
                    }
                    self.jobs().push(job.clone());
                    if !terminal {
                        crate::plog!("[chess {}] resuming after restart", job.id);
                        spawn_workers(&job, &self.factory);
                    }
                }
                Ok(None) => {}
                Err(err) => crate::plog!("[chess] cannot restore {}: {err}", dir.display()),
            }
        }
    }

    fn load_job(&self, dir: &Path) -> Result<Option<Arc<Job>>> {
        let Ok(text) = std::fs::read_to_string(dir.join("job.json")) else { return Ok(None) };
        let meta: Meta = serde_json::from_str(&text)?;
        let games: Vec<Game> = read_jsonl(&dir.join("games.jsonl"));
        let mut results: Vec<ResultItem> = read_jsonl(&dir.join("results.jsonl"));
        results.sort_by_key(|r| r.seq);
        let acked: u64 = std::fs::read_to_string(dir.join("acked")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let done: HashSet<&str> = results.iter().map(|r| r.game.as_str()).collect();
        let mut state = State::parse(&meta.state);
        let queue: VecDeque<Game> = games.iter().filter(|g| !done.contains(g.id.as_str())).cloned().collect();
        let next_seq = results.last().map_or(0, |r| r.seq);
        let found = results.iter().filter(|r| r.puzzle.is_some()).count() as u32;
        let scanned = results.len() as u32;
        let known: HashSet<String> = games.iter().map(|g| g.id.clone()).collect();
        let mut error = meta.error.clone();
        if !state.is_terminal() && meta.games_complete && queue.is_empty() {
            state = State::Done; // finished right as the daemon went down
        }
        if state == State::Done && !queue.is_empty() {
            state = State::Running;
            error = None;
        }
        results.retain(|r| r.seq > acked);
        Ok(Some(Arc::new(Job {
            id: meta.id,
            params: meta.params,
            cfg: meta.cfg,
            dir: dir.to_path_buf(),
            inner: Mutex::new(Inner {
                state,
                error,
                total: meta.total,
                known,
                queue,
                games_complete: meta.games_complete,
                scanned,
                found,
                errors: 0,
                results,
                next_seq,
                acked,
                active_workers: 0,
                scanned_at_start: scanned,
            }),
            cv: Condvar::new(),
            cancel: AtomicBool::new(false),
            notify: Notify::new(),
            started: Instant::now(),
        })))
    }
}

/// Appends one record as a single `write_all` (line + newline together) so concurrent
/// appenders can never interleave inside a line. Errors are returned: a record that is not on
/// disk must not be reported as durable.
fn append_line<T: Serialize>(path: &Path, rec: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_string(rec).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())
}

fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    // A torn last line (daemon killed mid-write) is skipped, not fatal.
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn age_days(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("job.json"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map_or(0, |d| d.as_secs() / 86_400)
}

// --- global registry ----------------------------------------------------------------------

/// The production registry: jobs under `<data>/chess/jobs`, Stockfish from `engines/`.
pub fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| {
        let factory: EngineFactory = Arc::new(|opts| {
            let path = super::install::find().ok_or_else(|| anyhow!("Stockfish is not installed"))?;
            Ok(Box::new(super::engine::Engine::spawn(&path, opts)?) as Box<dyn Analyzer + Send>)
        });
        Registry::new(crate::paths::data_dir().join("chess").join("jobs"), factory)
    })
}
