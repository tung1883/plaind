//! The `chess.*` messages: glue between a phone connection and the job [`registry`].
//!
//! The connection holds only the *subscription* (a pump task per channel); jobs themselves live
//! in the registry and keep running without it. See PROTOCOL.md ("Chess puzzles").

use rmpv::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

use super::generator::{Game, GenParams, Puzzle};
use super::jobs::{registry, Job, ResultItem, RunCfg, Snapshot};
use super::{engine::Engine, install};
use crate::proto::{self, Frame};

/// Per-connection state: which job each channel is subscribed to.
pub struct Conn {
    pumps: HashMap<i64, JoinHandle<()>>,
}

static INSTALLING: AtomicBool = AtomicBool::new(false);

/// Clears `INSTALLING` however the install task ends (including a panic).
struct InstallGuard;
impl Drop for InstallGuard {
    fn drop(&mut self) {
        INSTALLING.store(false, Ordering::SeqCst);
    }
}

fn int(v: i64) -> Value { Value::from(v) }
fn st(v: &str) -> Value { proto::s(v) }

fn err_msg(ch: i64, job: &str, code: &str, msg: &str) -> Value {
    proto::map(vec![
        ("t", st("chess.error")),
        ("ch", int(ch)),
        ("job", st(job)),
        ("code", st(code)),
        ("msg", st(msg)),
    ])
}

fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Map(m) => m.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v),
        _ => None,
    }
}

fn map_str(v: &Value, key: &str) -> String {
    map_get(v, key).and_then(|x| x.as_str()).unwrap_or_default().to_string()
}

fn map_f64(v: &Value, key: &str) -> Option<f64> {
    map_get(v, key).and_then(|x| x.as_f64().or_else(|| x.as_i64().map(|i| i as f64)).or_else(|| x.as_u64().map(|u| u as f64)))
}

fn map_u32(v: &Value, key: &str) -> Option<u32> {
    map_f64(v, key).filter(|f| *f >= 0.0).map(|f| f as u32)
}

pub fn parse_game(v: &Value) -> Option<Game> {
    let id = map_str(v, "id");
    if id.is_empty() {
        return None;
    }
    let sans = map_get(v, "sans")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default();
    Some(Game {
        id,
        src: map_str(v, "src"),
        white: map_str(v, "white"),
        black: map_str(v, "black"),
        event: map_str(v, "event"),
        date: map_str(v, "date"),
        sans,
    })
}

/// Generator thresholds/depths from a `params` map; anything missing keeps the PC default.
/// `strict: true` starts from the upstream lichess thresholds instead.
pub fn parse_params(v: Option<&Value>) -> GenParams {
    let Some(v) = v else { return GenParams::default() };
    let mut p = if map_get(v, "strict").and_then(|b| b.as_bool()) == Some(true) {
        GenParams::strict()
    } else {
        GenParams::default()
    };
    if let Some(x) = map_u32(v, "walk_depth") { p.walk_depth = x.clamp(1, 60); }
    if let Some(x) = map_u32(v, "pair_depth") { p.pair_depth = x.clamp(1, 60); }
    if let Some(x) = map_u32(v, "defense_depth") { p.defense_depth = x.clamp(1, 60); }
    if let Some(x) = map_u32(v, "walk_cap_ms") { p.walk_cap_ms = x; }
    if let Some(x) = map_u32(v, "deep_cap_ms") { p.deep_cap_ms = x; }
    if let Some(x) = map_f64(v, "swing") { p.swing = x.clamp(0.0, 2.0); }
    if let Some(x) = map_f64(v, "only_move_margin") { p.only_move_margin = x.clamp(0.0, 2.0); }
    if let Some(x) = map_u32(v, "min_ply") { p.min_ply = x; }
    if let Some(x) = map_f64(v, "mate_margin") { p.mate_margin = x.clamp(0.0, 2.0); }
    p
}

pub fn cores() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(2)
}

/// Run config from the request, clamped to what the machine can take.
pub fn parse_cfg(frame: &Frame) -> RunCfg {
    let cores = cores();
    let workers = proto::get_i64(frame, "workers").map(|w| w as u32).unwrap_or((cores / 2).max(1));
    RunCfg {
        workers: workers.clamp(1, cores.max(1)),
        threads: (proto::get_i64(frame, "threads").unwrap_or(1) as u32).clamp(1, cores.max(1)),
        hash_mb: (proto::get_i64(frame, "hash_mb").unwrap_or(128) as u32).clamp(16, 2048),
    }
}

pub fn puzzle_value(p: &Puzzle) -> Value {
    proto::map(vec![
        ("id", st(&p.id)),
        ("src", st(&p.src)),
        ("white", st(&p.white)),
        ("black", st(&p.black)),
        ("event", st(&p.event)),
        ("date", st(&p.date)),
        ("fen", st(&p.fen)),
        ("ply", int(p.ply as i64)),
        ("solution", Value::Array(p.solution.iter().map(|m| st(m)).collect())),
        ("winner_white", Value::Boolean(p.winner_white)),
        ("category", st(&p.category)),
        ("cp", int(p.cp as i64)),
    ])
}

fn results_msg(ch: i64, job: &str, items: &[ResultItem]) -> Value {
    let arr = items
        .iter()
        .map(|r| {
            let mut kv = vec![("seq", int(r.seq as i64)), ("game", st(&r.game))];
            if let Some(p) = &r.puzzle {
                kv.push(("puzzle", puzzle_value(p)));
            }
            proto::map(kv)
        })
        .collect();
    proto::map(vec![
        ("t", st("chess.results")),
        ("ch", int(ch)),
        ("job", st(job)),
        ("items", Value::Array(arr)),
    ])
}

fn snapshot_fields(s: &Snapshot) -> Vec<(&'static str, Value)> {
    let mut kv = vec![
        ("job", st(&s.id)),
        ("state", st(s.state.as_str())),
        ("total", int(s.total as i64)),
        ("have", int(s.have as i64)),
        ("games_complete", Value::Boolean(s.games_complete)),
        ("scanned", int(s.scanned as i64)),
        ("found", int(s.found as i64)),
        ("errors", int(s.errors as i64)),
        ("workers", int(s.workers as i64)),
        ("last_seq", int(s.last_seq as i64)),
        ("acked", int(s.acked as i64)),
        ("elapsed_s", int(s.elapsed_s as i64)),
        ("cfg_workers", int(s.cfg.workers as i64)),
        ("cfg_threads", int(s.cfg.threads as i64)),
        ("cfg_hash_mb", int(s.cfg.hash_mb as i64)),
    ];
    if let Some(e) = s.eta_s {
        kv.push(("eta_s", int(e as i64)));
    }
    if let Some(e) = &s.error {
        kv.push(("error", st(e)));
    }
    kv
}

fn progress_msg(ch: i64, s: &Snapshot) -> Value {
    let mut kv = vec![("t", st("chess.progress")), ("ch", int(ch))];
    kv.extend(snapshot_fields(s));
    proto::map(kv)
}

/// Streams a job's results and progress to one subscriber until the job ends (after the
/// last result) or the connection goes away. Everything it sends is derived from the job's
/// own state, so a missed wake-up costs at most one tick.
async fn pump(job: Arc<Job>, ch: i64, after: u64, tx: Sender<Value>) {
    let mut sent = after;
    let mut last_progress: Option<Instant> = None;
    let mut last_key = (u32::MAX, 0u32, 0u8, 0u32);
    loop {
        loop {
            let items = job.results_after(sent, 200);
            let Some(last) = items.last() else { break };
            sent = last.seq;
            if tx.send(results_msg(ch, &job.id, &items)).await.is_err() {
                return;
            }
        }
        let snap = job.snapshot();
        let key = (snap.scanned, snap.found, snap.state as u8, snap.have);
        let due = last_progress.map_or(true, |t| t.elapsed() >= Duration::from_secs(2));
        let drained = job.results_after(sent, 1).is_empty();
        let terminal = snap.state.is_terminal() && drained;
        if key != last_key || due || terminal {
            if tx.send(progress_msg(ch, &snap)).await.is_err() {
                return;
            }
            last_key = key;
            last_progress = Some(Instant::now());
        }
        if terminal {
            return;
        }
        tokio::select! {
            _ = job.notify.notified() => { tokio::time::sleep(Duration::from_millis(250)).await; }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

impl Conn {
    pub fn new() -> Conn {
        Conn { pumps: HashMap::new() }
    }

    /// Drop every subscription (connection closed). Jobs keep running.
    pub fn shutdown(&mut self) {
        for (_, h) in self.pumps.drain() {
            h.abort();
        }
    }

    fn subscribe(&mut self, job: Arc<Job>, ch: i64, after: u64, tx: &Sender<Value>) {
        if let Some(old) = self.pumps.remove(&ch) {
            old.abort();
        }
        self.pumps.insert(ch, tokio::spawn(pump(job, ch, after, tx.clone())));
    }

    pub async fn handle(&mut self, t: &str, frame: &Frame, ch: i64, tx: &Sender<Value>) {
        let job_id = proto::get_str(frame, "job").unwrap_or_default().to_string();
        match t {
            "chess.status" => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let info = tokio::task::spawn_blocking(|| {
                        let path = install::find();
                        let version = path.as_deref().and_then(Engine::id_name);
                        (path, version)
                    })
                    .await
                    .unwrap_or((None, None));
                    let mut kv = vec![
                        ("t", st("chess.status")),
                        ("ch", int(ch)),
                        ("installed", Value::Boolean(info.1.is_some())),
                        ("cores", int(cores() as i64)),
                        ("asset", st(install::asset_name())),
                        ("installing", Value::Boolean(INSTALLING.load(Ordering::Relaxed))),
                    ];
                    if let Some(p) = &info.0 {
                        kv.push(("path", st(&p.to_string_lossy())));
                    }
                    if let Some(v) = &info.1 {
                        kv.push(("version", st(v)));
                    }
                    if let Some(a) = registry().active() {
                        kv.push(("active_job", st(&a.id)));
                    }
                    tx.send(proto::map(kv)).await.ok();
                });
            }
            "chess.install" => {
                if INSTALLING.swap(true, Ordering::SeqCst) {
                    tx.send(err_msg(ch, "", "busy", "an install is already running")).await.ok();
                    return;
                }
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let _guard = InstallGuard;
                    let progress_tx = tx.clone();
                    let mut progress = |stage: &str, pct: u32| {
                        let _ = progress_tx.blocking_send(proto::map(vec![
                            ("t", st("chess.install.progress")),
                            ("ch", int(ch)),
                            ("stage", st(stage)),
                            ("pct", int(pct as i64)),
                        ]));
                    };
                    let result = install::install(&mut progress);
                    let msg = match result {
                        Ok(path) => {
                            let version = install::version(&path).unwrap_or_default();
                            crate::plog!("[chess] installed {version} at {}", path.display());
                            proto::map(vec![
                                ("t", st("chess.install.done")),
                                ("ch", int(ch)),
                                ("ok", Value::Boolean(true)),
                                ("path", st(&path.to_string_lossy())),
                                ("version", st(&version)),
                            ])
                        }
                        Err(e) => {
                            crate::plog!("[chess] install failed: {e}");
                            proto::map(vec![
                                ("t", st("chess.install.done")),
                                ("ch", int(ch)),
                                ("ok", Value::Boolean(false)),
                                ("err", st(&e.to_string())),
                            ])
                        }
                    };
                    let _ = tx.blocking_send(msg);
                });
            }
            "chess.job.start" => {
                if install::find().is_none() {
                    tx.send(err_msg(ch, &job_id, "no_engine", "Stockfish is not installed on this computer")).await.ok();
                    return;
                }
                let params = parse_params(proto::get(frame, "params"));
                let cfg = parse_cfg(frame);
                let total = proto::get_i64(frame, "total").unwrap_or(0).max(0) as u32;
                match registry().start(&job_id, params, cfg, total) {
                    Ok(job) => {
                        crate::plog!("[chess {}] started: {total} games, {cfg:?}", job.id);
                        let s = job.snapshot();
                        let mut kv = vec![("t", st("chess.job.started")), ("ch", int(ch))];
                        kv.extend(snapshot_fields(&s));
                        tx.send(proto::map(kv)).await.ok();
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let code = if msg.starts_with("busy") { "busy" } else { "bad_request" };
                        tx.send(err_msg(ch, &job_id, code, &msg)).await.ok();
                    }
                }
            }
            "chess.games" => {
                let Some(job) = registry().get(&job_id) else {
                    tx.send(err_msg(ch, &job_id, "no_job", "unknown job")).await.ok();
                    return;
                };
                let games: Vec<Game> = proto::get(frame, "games")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(parse_game).collect())
                    .unwrap_or_default();
                let have = job.add_games(games);
                tx.send(proto::map(vec![
                    ("t", st("chess.games.ack")),
                    ("ch", int(ch)),
                    ("job", st(&job_id)),
                    ("have", int(have as i64)),
                ]))
                .await
                .ok();
            }
            "chess.games.end" => match registry().get(&job_id) {
                Some(job) => job.end_games(),
                None => {
                    tx.send(err_msg(ch, &job_id, "no_job", "unknown job")).await.ok();
                }
            },
            "chess.job.list" => {
                let jobs: Vec<Value> = registry()
                    .list()
                    .iter()
                    .map(|j| proto::map(snapshot_fields(&j.snapshot())))
                    .collect();
                tx.send(proto::map(vec![
                    ("t", st("chess.job.list")),
                    ("ch", int(ch)),
                    ("jobs", Value::Array(jobs)),
                ]))
                .await
                .ok();
            }
            "chess.attach" => match registry().get(&job_id) {
                Some(job) => {
                    let after = proto::get_i64(frame, "after").unwrap_or(0).max(0) as u64;
                    self.subscribe(job, ch, after, tx);
                }
                None => {
                    tx.send(err_msg(ch, &job_id, "no_job", "unknown job")).await.ok();
                }
            },
            "chess.detach" => {
                if let Some(h) = self.pumps.remove(&ch) {
                    h.abort();
                }
            }
            "chess.ack" => {
                if let (Some(job), Some(upto)) = (registry().get(&job_id), proto::get_i64(frame, "upto")) {
                    job.ack(upto.max(0) as u64);
                }
            }
            "chess.cancel" => match registry().get(&job_id) {
                Some(job) => {
                    crate::plog!("[chess {}] cancel requested", job.id);
                    job.cancel();
                }
                None => {
                    tx.send(err_msg(ch, &job_id, "no_job", "unknown job")).await.ok();
                }
            },
            "chess.job.remove" => {
                let r = registry().remove(&job_id);
                tx.send(proto::map(vec![
                    ("t", st("chess.job.removed")),
                    ("ch", int(ch)),
                    ("job", st(&job_id)),
                    ("ok", Value::Boolean(r.is_ok())),
                    ("err", st(&r.err().map(|e| e.to_string()).unwrap_or_default())),
                ]))
                .await
                .ok();
            }
            other => {
                crate::plog!("chess: ignoring {other}");
            }
        }
    }
}
