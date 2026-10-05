//! Job lifecycle tests with a fake engine (no Stockfish needed): delivery log, acks, dedup,
//! busy/cancel, engine failure, and restore-after-restart from hand-built job files.

use anyhow::{bail, Result};
use plaind::chess::engine::{Analysis, Analyzer, EngineOpts};
use plaind::chess::generator::{Game, GenParams};
use plaind::chess::jobs::{EngineFactory, Registry, RunCfg, State};
use plaind::chess::score::Score;
use shakmaty::{CastlingMode, Chess, Position};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Always "equal" (Cp 0) — never triggers a puzzle, so only the job machinery is exercised.
struct Flat {
    delay_ms: u64,
    /// Fail the first `fail_calls` analyze() calls of this engine instance.
    fail_calls: u32,
    calls: u32,
}

impl Analyzer for Flat {
    fn analyze(&mut self, pos: &Chess, depth: u32, lines: u32, _movetime_ms: u32) -> Result<Vec<Analysis>> {
        self.calls += 1;
        if self.calls <= self.fail_calls {
            bail!("scripted engine failure");
        }
        if self.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.delay_ms));
        }
        Ok(pos
            .legal_moves()
            .iter()
            .take(lines as usize)
            .map(|m| Analysis {
                score: Score::Cp(0),
                depth,
                pv: vec![m.to_uci(CastlingMode::Standard).to_string()],
            })
            .collect())
    }
}

fn factory(delay_ms: u64, fail_calls: u32) -> EngineFactory {
    Arc::new(move |_opts: EngineOpts| Ok(Box::new(Flat { delay_ms, fail_calls, calls: 0 }) as Box<dyn Analyzer + Send>))
}

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("plaind-chess-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn game(i: usize) -> Game {
    Game {
        id: format!("g{i}"),
        src: "t.pgn".into(),
        white: "W".into(),
        black: "B".into(),
        event: "E".into(),
        date: "2020.01.01".into(),
        sans: ["e4", "e5", "Nf3", "Nc6"].iter().map(|s| s.to_string()).collect(),
    }
}

fn cfg(workers: u32) -> RunCfg {
    RunCfg { workers, threads: 1, hash_mb: 16 }
}

fn wait_terminal(job: &plaind::chess::jobs::Job, secs: u64) -> State {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let s = job.snapshot().state;
        if s.is_terminal() {
            return s;
        }
        assert!(Instant::now() < end, "job did not finish: {:?}", job.snapshot());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn runs_all_games_and_logs_each_once() {
    let reg = Registry::new(tmp("basic"), factory(0, 0));
    let job = reg.start("j1", GenParams::default(), cfg(3), 10).unwrap();
    assert_eq!(job.add_games((0..5).map(game).collect()), 5);
    assert_eq!(job.add_games((5..10).map(game).collect()), 10);
    job.end_games();
    assert_eq!(wait_terminal(&job, 20), State::Done);

    let s = job.snapshot();
    assert_eq!((s.scanned, s.found, s.errors, s.total, s.have), (10, 0, 0, 10, 10));
    assert_eq!(s.last_seq, 10);
    let r = job.results_after(0, 100);
    assert_eq!(r.len(), 10);
    let seqs: Vec<u64> = r.iter().map(|x| x.seq).collect();
    assert_eq!(seqs, (1..=10).collect::<Vec<_>>(), "seq is dense and ordered");
    let mut ids: Vec<&str> = r.iter().map(|x| x.game.as_str()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 10, "every game reported exactly once");
}

#[test]
fn duplicate_games_are_ignored() {
    let reg = Registry::new(tmp("dedup"), factory(0, 0));
    let job = reg.start("j1", GenParams::default(), cfg(1), 3).unwrap();
    job.add_games(vec![game(1), game(2)]);
    assert_eq!(job.add_games(vec![game(2), game(3), game(1)]), 3);
    job.end_games();
    assert_eq!(wait_terminal(&job, 20), State::Done);
    assert_eq!(job.snapshot().scanned, 3);
}

#[test]
fn ack_prunes_replay_log() {
    let reg = Registry::new(tmp("ack"), factory(0, 0));
    let job = reg.start("j1", GenParams::default(), cfg(2), 6).unwrap();
    job.add_games((0..6).map(game).collect());
    job.end_games();
    wait_terminal(&job, 20);
    assert_eq!(job.results_after(0, 100).len(), 6);
    job.ack(4);
    let left = job.results_after(0, 100);
    assert_eq!(left.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![5, 6]);
    job.ack(2); // stale ack: no effect
    assert_eq!(job.snapshot().acked, 4);
    job.ack(999); // beyond what exists: clamped
    assert_eq!(job.snapshot().acked, 6);
    assert!(job.results_after(0, 100).is_empty());
}

#[test]
fn one_running_job_at_a_time_and_start_is_idempotent() {
    let reg = Registry::new(tmp("busy"), factory(5, 0));
    let a = reg.start("a", GenParams::default(), cfg(1), 100).unwrap();
    let again = reg.start("a", GenParams::default(), cfg(1), 100).unwrap();
    assert!(Arc::ptr_eq(&a, &again));
    let err = reg.start("b", GenParams::default(), cfg(1), 1).err().expect("busy");
    assert!(err.to_string().starts_with("busy"), "{err}");
    a.cancel();
    wait_terminal(&a, 20);
    reg.start("b", GenParams::default(), cfg(1), 1).expect("free after the first ended");
}

#[test]
fn rejects_bad_job_ids() {
    let reg = Registry::new(tmp("ids"), factory(0, 0));
    for bad in ["", "../x", "a/b", "a b"] {
        assert!(reg.start(bad, GenParams::default(), cfg(1), 1).is_err(), "{bad:?}");
    }
}

#[test]
fn cancel_stops_promptly_and_never_records_half_done_games() {
    let reg = Registry::new(tmp("cancel"), factory(20, 0));
    let job = reg.start("j1", GenParams::default(), cfg(2), 200).unwrap();
    job.add_games((0..200).map(game).collect());
    job.end_games();
    let t0 = Instant::now();
    while job.snapshot().scanned < 3 {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    job.cancel();
    assert_eq!(wait_terminal(&job, 20), State::Cancelled);
    let s = job.snapshot();
    assert!(s.scanned < 200, "stopped early: {}", s.scanned);
    assert_eq!(s.scanned as usize, job.results_after(0, 1000).len(), "log matches the counter");
    assert_eq!(s.workers, 0);
}

#[test]
fn cancel_while_waiting_for_games_finishes_the_job() {
    let reg = Registry::new(tmp("cancel-idle"), factory(0, 0));
    let job = reg.start("j1", GenParams::default(), cfg(2), 50).unwrap();
    job.add_games(vec![game(0)]);
    std::thread::sleep(Duration::from_millis(200)); // workers now idle, waiting for more
    job.cancel();
    assert_eq!(wait_terminal(&job, 10), State::Cancelled);
}

#[test]
fn engine_that_cannot_start_fails_the_job() {
    let f: EngineFactory = Arc::new(|_| bail!("no stockfish here"));
    let reg = Registry::new(tmp("noengine"), f);
    let job = reg.start("j1", GenParams::default(), cfg(2), 4).unwrap();
    job.add_games((0..4).map(game).collect());
    job.end_games();
    assert_eq!(wait_terminal(&job, 20), State::Failed);
    let s = job.snapshot();
    assert!(s.error.unwrap().contains("no stockfish here"));
}

#[test]
fn crashed_engine_is_respawned_and_the_game_retried() {
    // Only the very first engine dies (on its first call); the respawned one must carry on.
    let spawned = Arc::new(AtomicU32::new(0));
    let sp = spawned.clone();
    let f: EngineFactory = Arc::new(move |_| {
        let n = sp.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Flat { delay_ms: 0, fail_calls: if n == 0 { 1 } else { 0 }, calls: 0 }) as Box<dyn Analyzer + Send>)
    });
    let reg = Registry::new(tmp("flaky"), f);
    let job = reg.start("j1", GenParams::default(), cfg(1), 3).unwrap();
    job.add_games((0..3).map(game).collect());
    job.end_games();
    assert_eq!(wait_terminal(&job, 20), State::Done);
    let s = job.snapshot();
    assert_eq!((s.scanned, s.errors), (3, 0));
    assert_eq!(spawned.load(Ordering::SeqCst), 2, "exactly one respawn");
}

#[test]
fn game_failing_twice_is_skipped_but_counted() {
    let spawned = Arc::new(AtomicU32::new(0));
    let sp = spawned.clone();
    // The first two engines die on their first call (game 0 burns both attempts); later ones work.
    let f: EngineFactory = Arc::new(move |_| {
        let n = sp.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Flat { delay_ms: 0, fail_calls: if n < 2 { 1 } else { 0 }, calls: 0 }) as Box<dyn Analyzer + Send>)
    });
    let reg = Registry::new(tmp("skip"), f);
    let job = reg.start("j1", GenParams::default(), cfg(1), 3).unwrap();
    job.add_games((0..3).map(game).collect());
    job.end_games();
    assert_eq!(wait_terminal(&job, 20), State::Done);
    let s = job.snapshot();
    assert_eq!(s.scanned, 3, "the failed game is still marked scanned so the phone moves on");
    assert_eq!(s.errors, 1);
}

#[test]
fn finished_job_can_be_removed_and_running_one_cannot() {
    let root = tmp("remove");
    let reg = Registry::new(root.clone(), factory(5, 0));
    let job = reg.start("j1", GenParams::default(), cfg(1), 100).unwrap();
    job.add_games((0..100).map(game).collect());
    assert!(reg.remove("j1").is_err(), "running");
    job.cancel();
    wait_terminal(&job, 20);
    assert!(root.join("j1").exists());
    reg.remove("j1").unwrap();
    assert!(!root.join("j1").exists());
    assert!(reg.get("j1").is_none());
    assert!(reg.remove("j1").is_err(), "already gone");
}

// --- persistence / restart ------------------------------------------------------------

fn write_job_files(root: &PathBuf, id: &str, state: &str, complete: bool, games: usize, done: usize, acked: u64, torn_tail: bool) {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = serde_json::json!({
        "id": id,
        "params": serde_json::to_value(GenParams::default()).unwrap(),
        "cfg": {"workers": 2, "threads": 1, "hash_mb": 16},
        "total": games,
        "games_complete": complete,
        "state": state,
        "error": null,
    });
    std::fs::write(dir.join("job.json"), meta.to_string()).unwrap();
    let mut g = String::new();
    for i in 0..games {
        g.push_str(&serde_json::to_string(&game(i)).unwrap());
        g.push('\n');
    }
    std::fs::write(dir.join("games.jsonl"), g).unwrap();
    let mut r = String::new();
    for i in 0..done {
        r.push_str(&serde_json::json!({"seq": i + 1, "game": format!("g{i}"), "puzzle": null}).to_string());
        r.push('\n');
    }
    if torn_tail {
        r.push_str("{\"seq\": 99, \"gam"); // daemon killed mid-write
    }
    std::fs::write(dir.join("results.jsonl"), r).unwrap();
    if acked > 0 {
        std::fs::write(dir.join("acked"), acked.to_string()).unwrap();
    }
}

#[test]
fn restart_resumes_unfinished_job_without_redoing_finished_games() {
    let root = tmp("resume");
    write_job_files(&root, "j1", "running", true, 12, 5, 3, true);
    let reg = Registry::new(root, factory(30, 0)); // slow enough to read the restored state first
    reg.restore(14);
    let job = reg.get("j1").expect("restored");
    let s0 = job.snapshot();
    assert_eq!((s0.scanned, s0.acked, s0.last_seq, s0.have), (5, 3, 5, 12));
    assert_eq!(wait_terminal(&job, 20), State::Done);
    let s = job.snapshot();
    assert_eq!(s.scanned, 12);
    assert_eq!(s.last_seq, 12, "seq continues after the restored ones");
    // unacked = seq 4,5 (restored) + 6..12 (new); the 5 done games are not analysed again
    let r = job.results_after(0, 100);
    assert_eq!(r.iter().map(|x| x.seq).collect::<Vec<_>>(), (4..=12).collect::<Vec<_>>());
    let mut ids: Vec<String> = r.iter().map(|x| x.game.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 9);
    assert!(!ids.contains(&"g0".to_string()) && !ids.contains(&"g2".to_string()), "acked/done games not re-sent");
}

#[test]
fn restart_keeps_finished_job_results_for_replay() {
    let root = tmp("replay");
    write_job_files(&root, "j1", "done", true, 8, 8, 3, false);
    let reg = Registry::new(root, factory(0, 0));
    reg.restore(14);
    let job = reg.get("j1").unwrap();
    let s = job.snapshot();
    assert_eq!(s.state, State::Done);
    assert_eq!(job.results_after(0, 100).iter().map(|x| x.seq).collect::<Vec<_>>(), vec![4, 5, 6, 7, 8]);
    assert!(reg.active().is_none(), "a done job does not block new ones");
}

#[test]
fn restart_with_games_still_arriving_waits_for_the_phone() {
    let root = tmp("await");
    write_job_files(&root, "j1", "running", false, 4, 2, 0, false);
    let reg = Registry::new(root, factory(0, 0));
    reg.restore(14);
    let job = reg.get("j1").unwrap();
    let t0 = Instant::now();
    while job.snapshot().scanned < 4 {
        assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", job.snapshot());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(job.snapshot().state, State::Running, "not done: upload incomplete");
    // phone reconnects, re-sends everything (dedup keeps the first 4) plus the rest, then ends
    let all: Vec<Game> = (0..7).map(game).collect();
    assert_eq!(job.add_games(all), 7);
    job.end_games();
    assert_eq!(wait_terminal(&job, 20), State::Done);
    assert_eq!(job.snapshot().scanned, 7);
}

#[test]
fn finished_job_that_lost_its_final_marker_is_resumed_not_lost() {
    // state says done, but some games have no result (e.g. state file written early): resume.
    let root = tmp("lost");
    write_job_files(&root, "j1", "done", true, 6, 4, 0, false);
    let reg = Registry::new(root, factory(0, 0));
    reg.restore(14);
    let job = reg.get("j1").unwrap();
    assert_eq!(wait_terminal(&job, 20), State::Done);
    assert_eq!(job.snapshot().scanned, 6);
}

#[test]
fn old_finished_jobs_are_pruned_on_restore() {
    let root = tmp("prune");
    write_job_files(&root, "old", "done", true, 2, 2, 2, false);
    let f = std::fs::File::options().write(true).open(root.join("old").join("job.json")).unwrap();
    f.set_modified(std::time::SystemTime::now() - Duration::from_secs(40 * 86_400)).unwrap();
    drop(f);
    write_job_files(&root, "recent", "done", true, 2, 2, 2, false);
    let reg = Registry::new(root.clone(), factory(0, 0));
    reg.restore(14);
    assert!(reg.get("old").is_none());
    assert!(!root.join("old").exists());
    assert!(reg.get("recent").is_some());
}

#[test]
fn garbage_job_dirs_do_not_break_restore() {
    let root = tmp("garbage");
    std::fs::create_dir_all(root.join("junk")).unwrap();
    std::fs::write(root.join("junk").join("job.json"), "not json").unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::write(root.join("stray.txt"), "x").unwrap();
    write_job_files(&root, "ok", "done", true, 1, 1, 0, false);
    let reg = Registry::new(root, factory(0, 0));
    reg.restore(14);
    assert!(reg.get("ok").is_some());
    assert_eq!(reg.list().len(), 1);
}

#[test]
fn state_survives_on_disk_across_a_real_run() {
    // Run a job to completion, then load the same root in a second registry (a "restart").
    let root = tmp("disk");
    let reg = Registry::new(root.clone(), factory(0, 0));
    let job = reg.start("j1", GenParams::default(), cfg(2), 6).unwrap();
    job.add_games((0..6).map(game).collect());
    job.end_games();
    wait_terminal(&job, 20);
    job.ack(2);

    let reg2 = Registry::new(root, factory(0, 0));
    reg2.restore(14);
    let j2 = reg2.get("j1").unwrap();
    let s = j2.snapshot();
    assert_eq!((s.state, s.scanned, s.acked, s.last_seq), (State::Done, 6, 2, 6));
    assert_eq!(j2.results_after(0, 100).iter().map(|r| r.seq).collect::<Vec<_>>(), vec![3, 4, 5, 6]);
}

// --- disk failures ----------------------------------------------------------------------

#[test]
fn unwritable_games_file_fails_the_job_instead_of_pretending() {
    let root = tmp("disk-games");
    let reg = Registry::new(root.clone(), factory(0, 0));
    let job = reg.start("j1", GenParams::default(), cfg(1), 3).unwrap();
    std::fs::remove_dir_all(root.join("j1")).unwrap(); // the disk "goes away"
    let have = job.add_games((0..3).map(game).collect());
    assert_eq!(have, 0, "nothing was accepted");
    assert_eq!(wait_terminal(&job, 10), State::Failed);
    let s = job.snapshot();
    assert!(s.error.unwrap().starts_with("disk:"), "error names the disk");
    assert_eq!(s.scanned, 0);
}

#[test]
fn unwritable_results_file_fails_the_job_and_hides_unsaved_results() {
    let root = tmp("disk-results");
    let reg = Registry::new(root.clone(), factory(40, 0));
    let job = reg.start("j1", GenParams::default(), cfg(1), 20).unwrap();
    job.add_games((0..20).map(game).collect());
    job.end_games();
    let t0 = Instant::now();
    while job.snapshot().scanned < 2 {
        assert!(t0.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::remove_dir_all(root.join("j1")).unwrap();
    assert_eq!(wait_terminal(&job, 20), State::Failed);
    let s = job.snapshot();
    assert!(s.error.unwrap().starts_with("disk:"));
    assert!(s.scanned < 20);
    assert_eq!(s.scanned as usize, job.results_after(0, 1000).len(), "only durable results are visible");
    assert_eq!(s.workers, 0, "workers stopped");
}

#[test]
fn a_failed_job_does_not_block_the_next_one() {
    let root = tmp("disk-next");
    let reg = Registry::new(root.clone(), factory(0, 0));
    let job = reg.start("j1", GenParams::default(), cfg(1), 1).unwrap();
    std::fs::remove_dir_all(root.join("j1")).unwrap();
    job.add_games(vec![game(0)]);
    wait_terminal(&job, 10);
    assert!(reg.active().is_none());
    reg.start("j2", GenParams::default(), cfg(1), 1).expect("a new job can start");
}
