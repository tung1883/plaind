//! Generator logic with a scripted engine: exact control over the evals the algorithm sees, so
//! each branch (advantage lines, trimming, swing/margin thresholds, repetition, errors) is
//! checked deterministically with no Stockfish.

use anyhow::{bail, Result};
use plaind::chess::engine::{Analysis, Analyzer};
use plaind::chess::generator::{parse_san, Game, GenParams, Generator, Puzzle};
use plaind::chess::score::Score;
use shakmaty::fen::Fen;
use shakmaty::{CastlingMode, Chess, EnPassantMode, Position};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

/// Answers from a FEN-keyed table (scores are from the side to move's POV, like a real engine);
/// unknown positions are dead equal. The "best move" is always the first legal move and the
/// "second" the next one, so the generator's chosen line is predictable.
#[derive(Default)]
struct Script {
    table: HashMap<String, Vec<Score>>,
    calls: u32,
    walk_calls: u32,
    fail_at_call: Option<u32>,
    /// FEN -> UCI of the move to report as best (instead of the first legal move).
    forced: HashMap<String, String>,
}

fn fen_of(p: &Chess) -> String {
    Fen::from_position(p, EnPassantMode::Legal).to_string()
}

impl Analyzer for Script {
    fn analyze(&mut self, pos: &Chess, depth: u32, lines: u32, _t: u32) -> Result<Vec<Analysis>> {
        self.calls += 1;
        if lines == 1 {
            self.walk_calls += 1;
        }
        if self.fail_at_call == Some(self.calls) {
            bail!("scripted failure");
        }
        let mut moves: Vec<shakmaty::Move> = pos.legal_moves().into_iter().collect();
        if let Some(u) = self.forced.get(&fen_of(pos)) {
            if let Some(i) = moves.iter().position(|m| &m.to_uci(CastlingMode::Standard).to_string() == u) {
                let m = moves.remove(i);
                moves.insert(0, m);
            }
        }
        let scores = self.table.get(&fen_of(pos)).cloned().unwrap_or_else(|| vec![Score::Cp(0), Score::Cp(0)]);
        Ok(moves
            .iter()
            .zip(scores.iter())
            .take(lines as usize)
            .map(|(m, s)| Analysis { score: *s, depth, pv: vec![m.to_uci(CastlingMode::Standard).to_string()] })
            .collect())
    }
}

const OPENING: &str = "e4 e5 Nf3 Nc6 Bc4 Bc5"; // 6 plies, White to move afterwards

fn game(moves: &str) -> Game {
    Game {
        id: "g".into(),
        src: "s".into(),
        white: "w".into(),
        black: "b".into(),
        event: "e".into(),
        date: "d".into(),
        sans: moves.split_whitespace().map(String::from).collect(),
    }
}

fn after(moves: &str) -> Chess {
    let mut p = Chess::default();
    for s in moves.split_whitespace() {
        let m = parse_san(&p, s).unwrap();
        p.play_unchecked(m);
    }
    p
}

fn first_move(p: &Chess) -> shakmaty::Move {
    p.legal_moves().first().unwrap().clone()
}

/// Positions along the generator's chosen line: start, then repeatedly "play the first legal move".
fn chain(start: &Chess, n: usize) -> Vec<Chess> {
    let mut v = vec![start.clone()];
    for _ in 0..n {
        let mut p = v.last().unwrap().clone();
        let m = first_move(&p);
        p.play_unchecked(m);
        v.push(p);
    }
    v
}

fn run(script: &mut Script, params: GenParams, g: &Game) -> Option<Puzzle> {
    let cancel = AtomicBool::new(false);
    Generator::new(script, params, &cancel).analyze_game(g).unwrap()
}

/// White to move at ply 6 with an advantage line of `winner_cps` for White's plies (and the
/// mirrored negative for Black's); `second` is White's second-best at each winner node.
fn advantage_script(walk_prev: i32, walk_now: i32, line: &[(i32, i32)]) -> (Script, Vec<Chess>) {
    let start = after(OPENING);
    let nodes = chain(&start, line.len() + 1);
    let mut s = Script::default();
    // prev (from ply 5, Black to move) is seen from Black's POV, so the table holds its negation.
    s.table.insert(fen_of(&after("e4 e5 Nf3 Nc6 Bc4")), vec![Score::Cp(-walk_prev), Score::Cp(0)]);
    s.table.insert(fen_of(&nodes[0]), vec![Score::Cp(walk_now), Score::Cp(line[0].1)]);
    for (i, (best, second)) in line.iter().enumerate() {
        let node = &nodes[i];
        let me_is_white = i % 2 == 0; // nodes[0] is White (the winner) to move
        let (b, sec) = if me_is_white { (*best, *second) } else { (-*best, 0) };
        s.table.insert(fen_of(node), vec![Score::Cp(b), Score::Cp(sec)]);
    }
    (s, nodes)
}

#[test]
fn advantage_puzzle_from_a_clear_swing_with_a_verified_only_move_line() {
    // White to move; prev +0, now +500. Winner plies: best 500/second 0 (huge gap), the last
    // winner ply has a close second so the line ends there.
    let line = [(500, 0), (480, 0), (450, 0), (440, 0), (430, 0), (420, 0), (410, 400)];
    let (mut s, nodes) = advantage_script(0, 500, &line);
    let g = game(OPENING);
    let p = run(&mut s, GenParams::default(), &g).expect("advantage puzzle");
    assert_eq!(p.category, "Advantage");
    assert!(p.winner_white);
    assert_eq!(p.ply, 6);
    assert_eq!(p.fen, fen_of(&nodes[0]));
    // 7 nodes analysed before the close-second one: solution is odd length, ends on a winner ply
    assert_eq!(p.solution.len() % 2, 1);
    assert!(p.solution.len() >= 3, "{:?}", p.solution);
    let expect: Vec<String> = nodes[..p.solution.len()]
        .iter()
        .map(|n| first_move(n).to_uci(CastlingMode::Standard).to_string())
        .collect();
    assert_eq!(p.solution, expect, "line is the engine's best move at every node");
    assert!(p.cp >= 200);
}

#[test]
fn line_ends_on_the_last_winner_ply_with_a_unique_answer() {
    // Winner nodes (even i): 0,2,4. Node 4 has a close second -> invalid attack -> line stops
    // after node 3; trimming drops the trailing defender ply, leaving moves from nodes 0..=2.
    let line = [(500, 0), (480, 0), (460, 0), (450, 0), (440, 430)];
    let (mut s, nodes) = advantage_script(0, 500, &line);
    let p = run(&mut s, GenParams::default(), &game(OPENING)).expect("puzzle");
    assert_eq!(p.solution.len(), 3);
    assert_eq!(p.solution[0], first_move(&nodes[0]).to_uci(CastlingMode::Standard).to_string());
    assert_eq!(p.cp, 460);
}

#[test]
fn one_mover_is_discarded() {
    // Only the first winner node is valid; the next winner node has a close second.
    let line = [(500, 0), (480, 0), (460, 450)];
    let (mut s, _) = advantage_script(0, 500, &line);
    // nodes: 0 W(500/0) 1 B 2 W(460/450 -> invalid) => solution [0,1] -> trimmed to [0] -> size 1
    assert!(run(&mut s, GenParams::default(), &game(OPENING)).is_none());
}

#[test]
fn two_move_puzzle_is_kept() {
    // [0,1,2,3] -> node 4 invalid -> pairs 0..3 -> even -> pop -> 3 moves ... wait: size 3 = two
    // winner moves. This is the case the phone used to throw away.
    let line = [(500, 0), (480, 0), (460, 0), (450, 0), (440, 435)];
    let (mut s, _) = advantage_script(0, 500, &line);
    let p = run(&mut s, GenParams::default(), &game(OPENING)).expect("a 2-mover is kept");
    assert_eq!(p.solution.len(), 3);
}

#[test]
fn swing_threshold_decides_between_relaxed_and_strict() {
    // prev +150 -> now +450: win-chance jump ~0.41 — enough for 0.4, not for 0.6.
    let line = [(450, 0), (440, 0), (430, 0), (420, 0), (410, 405)];
    let (mut s1, _) = advantage_script(150, 450, &line);
    assert!(run(&mut s1, GenParams::default(), &game(OPENING)).is_some(), "relaxed (0.4) accepts");
    let (mut s2, _) = advantage_script(150, 450, &line);
    assert!(run(&mut s2, GenParams::strict(), &game(OPENING)).is_none(), "upstream (0.6) rejects");
}

#[test]
fn only_move_margin_decides_between_relaxed_and_strict() {
    // Second-best +250 vs best +500: win-chance gap ~0.28+... choose values with gap between 0.5 and 0.7.
    // wc(500)=0.726, wc(100)=0.18 -> gap 0.54: ok for 0.5, not for 0.7.
    let line = [(500, 100), (480, 0), (460, 0), (450, 0), (440, 435)];
    let (mut s1, _) = advantage_script(0, 500, &line);
    assert!(run(&mut s1, GenParams::default(), &game(OPENING)).is_some());
    let (mut s2, _) = advantage_script(0, 500, &line);
    assert!(run(&mut s2, GenParams { only_move_margin: 0.7, ..GenParams::default() }, &game(OPENING)).is_none());
}

#[test]
fn default_margin_rejects_a_line_whose_alternative_is_nearly_as_good() {
    // best +500 (wc 0.73) vs second +300 (wc 0.50): gap 0.23, far below the 0.5 default.
    let line = [(500, 300), (480, 0), (460, 0), (450, 0), (440, 435)];
    let (mut s, _) = advantage_script(0, 500, &line);
    assert!(run(&mut s, GenParams::default(), &game(OPENING)).is_none());
    // ...and a permissive margin lets the same line through, proving the margin is what decided.
    let (mut s2, _) = advantage_script(0, 500, &line);
    assert!(run(&mut s2, GenParams { only_move_margin: 0.1, ..GenParams::default() }, &game(OPENING)).is_some());
}

#[test]
fn small_advantage_without_material_swing_is_not_a_puzzle() {
    // +300 is under the 400 bar and material is level -> "not clearly winning".
    let line = [(300, 0), (290, 0), (280, 0), (270, 0), (260, 255)];
    let (mut s, _) = advantage_script(0, 300, &line);
    assert!(run(&mut s, GenParams::default(), &game(OPENING)).is_none());
}

#[test]
fn already_winning_position_is_not_a_puzzle() {
    // prev was already +400 (> 300) and the new score is not a near mate: nothing new happened.
    let line = [(500, 0), (480, 0), (460, 0), (450, 0), (440, 435)];
    let (mut s, _) = advantage_script(400, 500, &line);
    assert!(run(&mut s, GenParams::default(), &game(OPENING)).is_none());
}

#[test]
fn line_that_loses_the_advantage_midway_aborts() {
    // Defender ply (node 3) shows the advantage has dropped under +2.00: the whole candidate is
    // discarded, not truncated.
    let line = [(500, 0), (480, 0), (460, 0), (150, 0), (140, 0)];
    let (mut s, _) = advantage_script(0, 500, &line);
    assert!(run(&mut s, GenParams::default(), &game(OPENING)).is_none());
}

#[test]
fn mate_in_one_is_too_easy() {
    let start = after(OPENING);
    let mut s = Script::default();
    s.table.insert(fen_of(&start), vec![Score::Mate(1), Score::Cp(0)]);
    assert!(run(&mut s, GenParams::default(), &game(OPENING)).is_none());
}

#[test]
fn unfinished_mate_line_is_never_accepted() {
    // The script claims mate in 2 but the line it walks never actually reaches a finished game.
    let start = after(OPENING);
    let nodes = chain(&start, 3);
    let mut s = Script::default();
    s.table.insert(fen_of(&nodes[0]), vec![Score::Mate(2), Score::Cp(0)]);
    s.table.insert(fen_of(&nodes[1]), vec![Score::Mate(-1), Score::Cp(-50)]);
    s.table.insert(fen_of(&nodes[2]), vec![Score::Mate(1), Score::Cp(-900)]);
    assert!(run(&mut s, GenParams::default(), &game(OPENING)).is_none());
}

const OPERA30: &str = "e4 e5 Nf3 d6 d4 Bg4 dxe5 Bxf3 Qxf3 dxe5 Bc4 Nf6 Qb3 Qe7 Nc3 c6 Bg5 b5 Nxb5 cxb5                        Bxb5+ Nbd7 O-O-O Rd8 Rxd7 Rxd7 Rd1 Qe6 Bxd7+ Nxd7";

#[test]
fn forced_mate_is_returned_with_the_full_line_and_mate_sentinel() {
    // The Opera Game finish, scripted: Qb8+ (mate in 2), Nxb8 forced, Rd8#.
    let p30 = after(OPERA30);
    let p31 = after(&format!("{OPERA30} Qb8+"));
    let p32 = after(&format!("{OPERA30} Qb8+ Nxb8"));
    let mut s = Script::default();
    s.table.insert(fen_of(&p30), vec![Score::Mate(2), Score::Cp(300)]);
    s.forced.insert(fen_of(&p30), "b3b8".into());
    s.table.insert(fen_of(&p31), vec![Score::Mate(-1), Score::Cp(-9000)]);
    s.forced.insert(fen_of(&p31), "d7b8".into());
    s.table.insert(fen_of(&p32), vec![Score::Mate(1), Score::Cp(-800)]);
    s.forced.insert(fen_of(&p32), "d1d8".into());
    let p = run(&mut s, GenParams::default(), &game(OPERA30)).expect("mate puzzle");
    assert_eq!(p.category, "Mate");
    assert_eq!(p.ply, 30);
    assert_eq!(p.solution, vec!["b3b8", "d7b8", "d1d8"]);
    assert_eq!(p.cp, i32::MAX - 1);
    assert!(p.winner_white);
    assert_eq!(p.fen, fen_of(&p30));
}

#[test]
fn mate_with_a_near_equal_alternative_is_rejected_at_the_upstream_margin_only() {
    // best Mate(2) (win chance 1.0) vs alternative +333 (0.55): gap 0.45 — fine at the relaxed
    // mate margin (0.4), rejected at upstream's 0.7.
    let build = || {
        let p30 = after(OPERA30);
        let p31 = after(&format!("{OPERA30} Qb8+"));
        let p32 = after(&format!("{OPERA30} Qb8+ Nxb8"));
        let mut s = Script::default();
        s.table.insert(fen_of(&p30), vec![Score::Mate(2), Score::Cp(333)]);
        s.forced.insert(fen_of(&p30), "b3b8".into());
        s.table.insert(fen_of(&p31), vec![Score::Mate(-1), Score::Cp(-9000)]);
        s.forced.insert(fen_of(&p31), "d7b8".into());
        s.table.insert(fen_of(&p32), vec![Score::Mate(1), Score::Cp(-800)]);
        s.forced.insert(fen_of(&p32), "d1d8".into());
        s
    };
    assert!(run(&mut build(), GenParams::default(), &game(OPERA30)).is_some());
    assert!(run(&mut build(), GenParams::strict(), &game(OPERA30)).is_none());
}

#[test]
fn repeated_positions_are_not_walked_twice() {
    let g = game("Nf3 Nf6 Ng1 Ng8 Nf3 Nf6 Ng1 Ng8 Nf3 Nf6");
    let mut s = Script::default();
    assert!(run(&mut s, GenParams::default(), &g).is_none());
    // 10 plies; plies 4,5.. repeat earlier positions and are skipped until something irreversible
    assert!(s.walk_calls < 10, "walk evaluated {} of 10 plies", s.walk_calls);
    assert!(s.walk_calls >= 3);
}

#[test]
fn min_ply_primes_the_baseline_instead_of_flagging_a_phantom_swing() {
    // Walk from ply 4 on; the first evaluated ply must only seed `prev`, otherwise an immediately
    // good score would look like a huge swing from the default +20 baseline.
    let start = after(OPENING);
    let line = [(500, 0), (480, 0), (460, 0), (450, 0), (440, 435)];
    let (mut s, _) = advantage_script(0, 500, &line);
    // min_ply 6: ply 6 is the first evaluated ply -> baseline only -> no candidate there
    let p = run(&mut s, GenParams { min_ply: 6, ..GenParams::default() }, &game(OPENING));
    assert!(p.is_none());
    let _ = start;
}

#[test]
fn engine_error_propagates_instead_of_being_swallowed() {
    let mut s = Script { fail_at_call: Some(2), ..Script::default() };
    let cancel = AtomicBool::new(false);
    let r = Generator::new(&mut s, GenParams::default(), &cancel).analyze_game(&game(OPENING));
    assert!(r.is_err());
}

#[test]
fn castling_and_annotated_san_are_read() {
    let p = after("e4 e5 Nf3 Nc6 Bc4 Bc5");
    for san in ["O-O", "0-0", "O-O+", "Nc3!?", "d3?!"] {
        assert!(parse_san(&p, san).is_some(), "{san}");
    }
    assert!(parse_san(&p, "Qxx9").is_none());
    assert!(parse_san(&p, "").is_none());
}

#[test]
fn promotion_uci_carries_the_piece_letter() {
    // 8/P6k/8/8/8/8/8/K7 w: a7-a8=Q is legal; the SAN reader must resolve it and the move's UCI
    // must be a7a8q, not the phone-style 4-char "a7a8".
    let p: Chess = "8/P6k/8/8/8/8/8/K7 w - - 0 1".parse::<Fen>().unwrap().into_position(CastlingMode::Standard).unwrap();
    let m = parse_san(&p, "a8=Q").expect("promotion san");
    assert_eq!(m.to_uci(CastlingMode::Standard).to_string(), "a7a8q");
    let m2 = parse_san(&p, "a8=N+").expect("underpromotion san with annotation");
    assert_eq!(m2.to_uci(CastlingMode::Standard).to_string(), "a7a8n");
}
