//! Tests against a real Stockfish: installer, UCI wrapper, the generator on known games, and the
//! full `chess.*` wire flow (start → upload → attach → results → ack → replay).
//!
//! Stockfish is installed once into `target/chess-test-data/engines` (needs network the first
//! time) and reused after that. The wire tests share the process-global job registry, so they
//! run one at a time behind `SERIAL`.

use plaind::chess::engine::{Analyzer, Engine, EngineOpts};
use plaind::chess::generator::{Game, GenParams, Generator, Puzzle};
use plaind::chess::score::Score;
use plaind::chess::wire::Conn;
use plaind::chess::{install, jobs};
use plaind::proto;
use rmpv::Value;
use shakmaty::fen::Fen;
use shakmaty::{CastlingMode, Chess, Position};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

static SERIAL: Mutex<()> = Mutex::new(());
static SETUP: Once = Once::new();

fn engine_path() -> PathBuf {
    SETUP.call_once(|| {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("chess-test-data");
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("PLAIND_DATA", &dir);
        if install::find().is_none() {
            install::install(&mut |stage, pct| eprintln!("install: {stage} {pct}%")).expect("install stockfish");
        }
    });
    install::find().expect("stockfish present")
}

fn spawn(threads: u32) -> Engine {
    Engine::spawn(&engine_path(), EngineOpts { threads, hash_mb: 64 }).expect("engine")
}

fn pos(fen: &str) -> Chess {
    fen.parse::<Fen>().unwrap().into_position(CastlingMode::Standard).unwrap()
}

fn san(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn game(id: &str, moves: &str) -> Game {
    Game {
        id: id.into(),
        src: "test.pgn".into(),
        white: "White".into(),
        black: "Black".into(),
        event: "Test".into(),
        date: "2020.01.01".into(),
        sans: san(moves),
    }
}

/// Morphy vs Duke of Brunswick & Count Isouard, 1858. After 15...Nxd7 (ply 30) White has the
/// famous 16.Qb8+! Nxb8 17.Rd8#.
const OPERA: &str = "e4 e5 Nf3 d6 d4 Bg4 dxe5 Bxf3 Qxf3 dxe5 Bc4 Nf6 Qb3 Qe7 Nc3 c6 Bg5 b5 Nxb5 cxb5 \
                     Bxb5+ Nbd7 O-O-O Rd8 Rxd7 Rxd7 Rd1 Qe6 Bxd7+ Nxd7 Qb8+ Nxb8 Rd8#";

/// A calm, balanced opening — nothing here is a puzzle.
const QUIET: &str = "Nf3 Nf6 g3 g6 Bg2 Bg7 O-O O-O d3 d6 c4 c5 Nc3 Nc6 a3 a6 Rb1 Rb8 b4 cxb4 axb4 b5 cxb5 axb5 Bd2 Bd7";

fn fast() -> GenParams {
    // Shallow but real: enough to see the Opera mate, quick enough for CI.
    GenParams { walk_depth: 12, pair_depth: 14, defense_depth: 10, walk_cap_ms: 2_000, deep_cap_ms: 5_000, ..GenParams::default() }
}

fn run_game(g: &Game, params: GenParams) -> Option<Puzzle> {
    let mut e = spawn(1);
    let cancel = AtomicBool::new(false);
    Generator::new(&mut e, params, &cancel).analyze_game(g).expect("analysis")
}

// --- installer ------------------------------------------------------------------------

#[test]
fn installer_provides_a_working_stockfish() {
    let p = engine_path();
    assert!(p.is_file());
    let v = install::version(&p).expect("engine answers `uci`");
    assert!(v.to_lowercase().contains("stockfish"), "{v}");
    assert!(p.starts_with(install::engines_dir()) || std::env::var_os("PLAIND_STOCKFISH").is_some());
}

// --- UCI wrapper ----------------------------------------------------------------------

#[test]
fn engine_finds_mate_in_one_and_reports_pov() {
    let mut e = spawn(1);
    let p = pos("6k1/5ppp/8/8/8/8/5PPP/3R2K1 w - - 0 1");
    let lines = e.analyze(&p, 10, 1, 0).unwrap();
    assert_eq!(lines[0].score, Score::Mate(1));
    assert_eq!(lines[0].pv[0], "d1d8");
}

#[test]
fn engine_multipv_returns_ranked_lines() {
    let mut e = spawn(1);
    let lines = e.analyze(&Chess::default(), 10, 3, 0).unwrap();
    assert_eq!(lines.len(), 3);
    let mut seen: Vec<&str> = lines.iter().map(|l| l.pv[0].as_str()).collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3, "three distinct first moves");
    assert!(lines[0].score.key() >= lines[2].score.key(), "best first");
}

#[test]
fn engine_returns_nothing_for_a_finished_game_and_survives_it() {
    let mut e = spawn(1);
    let mated = pos("3R2k1/5ppp/8/8/8/8/5PPP/6K1 b - - 0 1"); // black is checkmated
    assert!(mated.legal_moves().is_empty());
    assert!(e.analyze(&mated, 8, 1, 0).unwrap().is_empty());
    // same process keeps working afterwards
    assert_eq!(e.analyze(&Chess::default(), 6, 1, 0).unwrap().len(), 1);
}

#[test]
fn movetime_cap_bounds_a_deep_search() {
    let mut e = spawn(1);
    let t0 = Instant::now();
    let r = e.analyze(&Chess::default(), 60, 1, 300).unwrap();
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    assert!(!r.is_empty());
}

#[test]
fn multipv_setting_follows_each_call() {
    let mut e = spawn(1);
    assert_eq!(e.analyze(&Chess::default(), 6, 2, 0).unwrap().len(), 2);
    assert_eq!(e.analyze(&Chess::default(), 6, 1, 0).unwrap().len(), 1);
    assert_eq!(e.analyze(&Chess::default(), 6, 4, 0).unwrap().len(), 4);
}

#[test]
fn engine_that_cannot_launch_is_a_clean_error() {
    let r = Engine::spawn(std::path::Path::new("Z:/definitely/not/here.exe"), EngineOpts { threads: 1, hash_mb: 16 });
    assert!(r.is_err());
}

// --- generator on real games -------------------------------------------------------------

#[test]
fn san_replay_reaches_the_mate_in_the_opera_game() {
    let mut p = Chess::default();
    for s in san(OPERA) {
        let m = plaind::chess::generator::parse_san(&p, &s).unwrap_or_else(|| panic!("cannot read {s}"));
        p.play_unchecked(m);
    }
    assert!(p.is_checkmate());
}

#[test]
fn opera_game_yields_a_mate_puzzle_with_the_famous_queen_sacrifice() {
    let p = run_game(&game("opera", OPERA), fast()).expect("a puzzle in the Opera Game");
    assert_eq!(p.category, "Mate");
    assert!(p.winner_white, "White is the winning side");
    assert_eq!(p.ply % 2, 0, "puzzle position arises after a Black move");
    // The solution must be a legal line from the puzzle FEN ending in checkmate.
    let mut b = pos(&p.fen);
    for (i, u) in p.solution.iter().enumerate() {
        let m = shakmaty::uci::UciMove::from_ascii(u.as_bytes()).unwrap().to_move(&b).unwrap_or_else(|_| panic!("illegal {u} at {i}"));
        b.play_unchecked(m);
    }
    assert!(b.is_checkmate(), "solution ends in mate: {:?}", p.solution);
    assert_eq!(p.solution.len() % 2, 1, "winner moves first and last");
    assert_eq!(p.cp, i32::MAX - 1);
    assert_eq!((p.src.as_str(), p.white.as_str(), p.event.as_str()), ("test.pgn", "White", "Test"));
    assert_eq!(p.id.len(), 36);
}

#[test]
fn opera_puzzle_starts_at_the_queen_sacrifice_position() {
    let p = run_game(&game("opera", OPERA), fast()).expect("puzzle");
    // 16.Qb8+ is the key move whichever earlier ply the generator picked it up at.
    assert_eq!(p.ply, 30, "found at 15...Nxd7 (first position with a verified forced mate), got ply {}", p.ply);
    assert_eq!(p.solution, vec!["b3b8", "d7b8", "d1d8"]);
}

#[test]
fn upstream_mate_margin_rejects_the_opera_mate_because_a_plus_three_alternative_exists() {
    // At ply 30 the engine sees Mate(2) against a +3.3 alternative. Upstream's 0.7 gap demands
    // the alternative be near-equal, so the puzzle is (deliberately) not produced in strict mode.
    let strict = GenParams { mate_margin: 0.7, ..fast() };
    let p = run_game(&game("opera", OPERA), strict);
    assert!(p.map_or(true, |p| p.ply != 30));
}

/// The real PC defaults (walk depth 20, pair depth 22): timing probe, opt-in because it is slow.
/// `cargo test --test chess_engine deep_defaults -- --ignored --nocapture`
/// No puzzle is asserted: at depth 22 the Opera mate has a +5.5 alternative, so it is no longer
/// an "only move" — deeper search finds more winning alternatives, which is correct.
#[test]
#[ignore]
fn deep_defaults_timing() {
    for (name, moves) in [("opera", OPERA), ("quiet", QUIET)] {
        let g = game(name, moves);
        let t0 = Instant::now();
        let p = run_game(&g, GenParams::default());
        eprintln!("{name} ({} plies) @ defaults: {:?} -> {:?}", g.sans.len(), t0.elapsed(), p.map(|p| (p.category, p.ply)));
    }
}

#[test]
fn quiet_game_has_no_puzzle() {
    assert!(run_game(&game("quiet", QUIET), fast()).is_none());
}

#[test]
fn unreadable_move_stops_the_walk_without_error() {
    let g = game("bad", "e4 e5 Nf3 Zz9 Nc6");
    assert!(run_game(&g, fast()).is_none());
}

#[test]
fn empty_game_is_fine() {
    assert!(run_game(&game("empty", ""), fast()).is_none());
}

#[test]
fn cancel_flag_aborts_a_game_quickly() {
    let mut e = spawn(1);
    let cancel = AtomicBool::new(true);
    let t0 = Instant::now();
    let r = Generator::new(&mut e, fast(), &cancel).analyze_game(&game("opera", OPERA)).unwrap();
    assert!(r.is_none());
    assert!(t0.elapsed() < Duration::from_secs(5));
}

#[test]
fn min_ply_skips_the_opening_but_still_finds_the_late_puzzle() {
    let p = run_game(&game("opera", OPERA), GenParams { min_ply: 20, ..fast() }).expect("puzzle");
    assert!(p.ply >= 20);
}

#[test]
fn strict_thresholds_are_the_upstream_values() {
    let s = GenParams::strict();
    assert_eq!((s.swing, s.only_move_margin, s.mate_margin), (0.6, 0.7, 0.7));
    let d = GenParams::default();
    assert_eq!((d.swing, d.only_move_margin, d.mate_margin), (0.4, 0.5, 0.4));
    assert_eq!((d.walk_depth, d.pair_depth), (20, 22), "PC defaults are deep");
}

// --- wire flow ------------------------------------------------------------------------------

fn get<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    match v {
        Value::Map(m) => m.iter().find(|(kk, _)| kk.as_str() == Some(k)).map(|(_, v)| v),
        _ => None,
    }
}
fn gs<'a>(v: &'a Value, k: &str) -> &'a str { get(v, k).and_then(|x| x.as_str()).unwrap_or("") }
fn gi(v: &Value, k: &str) -> i64 { get(v, k).and_then(|x| x.as_i64()).unwrap_or(-1) }

fn frame(kv: Vec<(&str, Value)>) -> proto::Frame {
    match proto::map(kv) {
        Value::Map(m) => m,
        _ => unreachable!(),
    }
}

fn game_value(g: &Game) -> Value {
    proto::map(vec![
        ("id", proto::s(&g.id)),
        ("src", proto::s(&g.src)),
        ("white", proto::s(&g.white)),
        ("black", proto::s(&g.black)),
        ("event", proto::s(&g.event)),
        ("date", proto::s(&g.date)),
        ("sans", Value::Array(g.sans.iter().map(|s| proto::s(s)).collect())),
    ])
}

async fn send(conn: &mut Conn, tx: &mpsc::Sender<Value>, ch: i64, t: &str, mut kv: Vec<(&str, Value)>) {
    kv.push(("t", proto::s(t)));
    kv.push(("ch", Value::from(ch)));
    let f = frame(kv);
    conn.handle(t, &f, ch, tx).await;
}

async fn next_of(rx: &mut mpsc::Receiver<Value>, t: &str, secs: u64) -> Value {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(v)) if gs(&v, "t") == t => return v,
            Ok(Some(_)) => {}
            _ => panic!("timed out waiting for {t}"),
        }
    }
}

/// Collects `chess.results` items until a terminal `chess.progress` arrives.
async fn drain_until_done(rx: &mut mpsc::Receiver<Value>, secs: u64) -> (Vec<Value>, Value) {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut items = Vec::new();
    loop {
        let left = end.saturating_duration_since(Instant::now());
        let v = match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(v)) => v,
            _ => panic!("timed out; got {} items", items.len()),
        };
        match gs(&v, "t") {
            "chess.results" => {
                if let Some(Value::Array(a)) = get(&v, "items") {
                    items.extend(a.iter().cloned());
                }
            }
            "chess.progress" if gs(&v, "state") != "running" => return (items, v),
            _ => {}
        }
    }
}

fn fast_params() -> Value {
    proto::map(vec![
        ("walk_depth", Value::from(12)),
        ("pair_depth", Value::from(14)),
        ("defense_depth", Value::from(10)),
        ("walk_cap_ms", Value::from(2000)),
        ("deep_cap_ms", Value::from(5000)),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn wire_flow_start_upload_attach_results_ack_replay() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    engine_path();
    let reg = jobs::registry();
    if let Some(a) = reg.active() {
        a.cancel();
    }
    let job_id = format!("wire-{}", std::process::id());

    let (tx, mut rx) = mpsc::channel::<Value>(64);
    let mut conn = Conn::new();

    // status
    send(&mut conn, &tx, 7, "chess.status", vec![]).await;
    let st = next_of(&mut rx, "chess.status", 20).await;
    assert_eq!(get(&st, "installed").and_then(|v| v.as_bool()), Some(true));
    assert!(gs(&st, "version").to_lowercase().contains("stockfish"));
    assert!(gi(&st, "cores") >= 1);

    // start + upload in two chunks + end
    send(&mut conn, &tx, 7, "chess.job.start", vec![
        ("job", proto::s(&job_id)),
        ("params", fast_params()),
        ("workers", Value::from(2)),
        ("total", Value::from(3)),
    ]).await;
    let started = next_of(&mut rx, "chess.job.started", 10).await;
    assert_eq!(gs(&started, "job"), job_id);

    let games = [game("opera", OPERA), game("quiet", QUIET), game("short", "e4 e5 Nf3 Nc6")];
    send(&mut conn, &tx, 7, "chess.games", vec![
        ("job", proto::s(&job_id)),
        ("games", Value::Array(games[..2].iter().map(game_value).collect())),
    ]).await;
    assert_eq!(gi(&next_of(&mut rx, "chess.games.ack", 10).await, "have"), 2);
    send(&mut conn, &tx, 7, "chess.games", vec![
        ("job", proto::s(&job_id)),
        ("games", Value::Array(games[1..].iter().map(game_value).collect())), // quiet re-sent: dedup
    ]).await;
    assert_eq!(gi(&next_of(&mut rx, "chess.games.ack", 10).await, "have"), 3);
    send(&mut conn, &tx, 7, "chess.games.end", vec![("job", proto::s(&job_id))]).await;

    // a second job while this one runs is refused
    send(&mut conn, &tx, 7, "chess.job.start", vec![("job", proto::s("other")), ("total", Value::from(1))]).await;
    let busy = next_of(&mut rx, "chess.error", 10).await;
    assert_eq!(gs(&busy, "code"), "busy");

    // attach and stream to the end
    send(&mut conn, &tx, 7, "chess.attach", vec![("job", proto::s(&job_id)), ("after", Value::from(0))]).await;
    let (items, fin) = drain_until_done(&mut rx, 180).await;
    assert_eq!(gs(&fin, "state"), "done");
    assert_eq!((gi(&fin, "scanned"), gi(&fin, "found"), gi(&fin, "errors")), (3, 1, 0));
    assert_eq!(items.len(), 3);
    let mut seqs: Vec<i64> = items.iter().map(|i| gi(i, "seq")).collect();
    seqs.sort();
    assert_eq!(seqs, vec![1, 2, 3]);
    let with_puzzle: Vec<&Value> = items.iter().filter(|i| get(i, "puzzle").is_some()).collect();
    assert_eq!(with_puzzle.len(), 1);
    assert_eq!(gs(with_puzzle[0], "game"), "opera");
    let pz = get(with_puzzle[0], "puzzle").unwrap();
    assert_eq!(gs(pz, "category"), "Mate");
    assert_eq!(get(pz, "winner_white").and_then(|v| v.as_bool()), Some(true));
    assert!(matches!(get(pz, "solution"), Some(Value::Array(a)) if a.len() == 3));

    // phone "disconnects" without acking: a fresh connection is replayed everything
    conn.shutdown();
    drop(conn);
    let (tx2, mut rx2) = mpsc::channel::<Value>(64);
    let mut conn2 = Conn::new();
    send(&mut conn2, &tx2, 9, "chess.job.list", vec![]).await;
    let list = next_of(&mut rx2, "chess.job.list", 10).await;
    let Some(Value::Array(jobs)) = get(&list, "jobs") else { panic!("no jobs") };
    let mine = jobs.iter().find(|j| gs(j, "job") == job_id).expect("job still listed");
    assert_eq!((gs(mine, "state"), gi(mine, "last_seq"), gi(mine, "acked")), ("done", 3, 0));
    send(&mut conn2, &tx2, 9, "chess.attach", vec![("job", proto::s(&job_id)), ("after", Value::from(0))]).await;
    let (replayed, _) = drain_until_done(&mut rx2, 20).await;
    assert_eq!(replayed.len(), 3, "everything unacked is replayed");

    // ack the first two; a later attach only sees the rest
    send(&mut conn2, &tx2, 9, "chess.ack", vec![("job", proto::s(&job_id)), ("upto", Value::from(2))]).await;
    conn2.shutdown();
    let (tx3, mut rx3) = mpsc::channel::<Value>(64);
    let mut conn3 = Conn::new();
    send(&mut conn3, &tx3, 1, "chess.attach", vec![("job", proto::s(&job_id)), ("after", Value::from(0))]).await;
    let (rest, _) = drain_until_done(&mut rx3, 20).await;
    assert_eq!(rest.iter().map(|i| gi(i, "seq")).collect::<Vec<_>>(), vec![3]);

    // finished job can be removed
    send(&mut conn3, &tx3, 1, "chess.job.remove", vec![("job", proto::s(&job_id))]).await;
    let rm = next_of(&mut rx3, "chess.job.removed", 10).await;
    assert_eq!(get(&rm, "ok").and_then(|v| v.as_bool()), Some(true));
    conn3.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn wire_cancel_mid_run_ends_cancelled() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    engine_path();
    let reg = jobs::registry();
    if let Some(a) = reg.active() {
        a.cancel();
    }
    let job_id = format!("cancel-{}", std::process::id());
    let (tx, mut rx) = mpsc::channel::<Value>(64);
    let mut conn = Conn::new();
    send(&mut conn, &tx, 1, "chess.job.start", vec![
        ("job", proto::s(&job_id)),
        ("params", proto::map(vec![("walk_depth", Value::from(14)), ("walk_cap_ms", Value::from(1000))])),
        ("workers", Value::from(1)),
        ("total", Value::from(40)),
    ]).await;
    next_of(&mut rx, "chess.job.started", 10).await;
    let many: Vec<Value> = (0..40).map(|i| game_value(&game(&format!("q{i}"), QUIET))).collect();
    send(&mut conn, &tx, 1, "chess.games", vec![("job", proto::s(&job_id)), ("games", Value::Array(many))]).await;
    send(&mut conn, &tx, 1, "chess.games.end", vec![("job", proto::s(&job_id))]).await;
    send(&mut conn, &tx, 1, "chess.attach", vec![("job", proto::s(&job_id)), ("after", Value::from(0))]).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    send(&mut conn, &tx, 1, "chess.cancel", vec![("job", proto::s(&job_id))]).await;
    let (_, fin) = drain_until_done(&mut rx, 60).await;
    assert_eq!(gs(&fin, "state"), "cancelled");
    assert!(gi(&fin, "scanned") < 40);
    conn.shutdown();
    let _ = reg.remove(&job_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn wire_start_without_engine_is_a_clean_error() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    engine_path();
    let reg = jobs::registry();
    if let Some(a) = reg.active() {
        a.cancel();
    }
    // Unknown job ids on every verb that takes one.
    let (tx, mut rx) = mpsc::channel::<Value>(16);
    let mut conn = Conn::new();
    for t in ["chess.games", "chess.games.end", "chess.attach", "chess.cancel"] {
        send(&mut conn, &tx, 3, t, vec![("job", proto::s("nope"))]).await;
        let e = next_of(&mut rx, "chess.error", 10).await;
        assert_eq!(gs(&e, "code"), "no_job", "{t}");
    }
    send(&mut conn, &tx, 3, "chess.job.start", vec![("job", proto::s("../evil")), ("total", Value::from(1))]).await;
    let e = next_of(&mut rx, "chess.error", 10).await;
    assert_eq!(gs(&e, "code"), "bad_request");
}

#[test]
fn param_parsing_clamps_and_defaults() {
    use plaind::chess::wire::parse_params;
    assert_eq!(parse_params(None), GenParams::default());
    let v = proto::map(vec![("walk_depth", Value::from(999)), ("swing", Value::from(9.0)), ("strict", Value::Boolean(true))]);
    let p = parse_params(Some(&v));
    assert_eq!(p.walk_depth, 60);
    assert_eq!(p.swing, 2.0);
    assert_eq!(p.only_move_margin, 0.7, "strict base applied for fields not given");
}
