//! Rust port of the phone's `PuzzleGenerator.java`, itself a port of lichess-puzzler's
//! generator.py: walk a game ply by ply, flag a win-chance swing, then verify a forced line
//! ("only move" for the winner) — a mate line or an advantage line. Differences from the
//! phone: the board is shakmaty (promotions are real, not auto-queen, so solution UCI can
//! carry a promotion letter) and the thresholds/depths come from [`GenParams`].

use anyhow::Result;
use serde::{Deserialize, Serialize};
use shakmaty::fen::Fen;
use shakmaty::san::San;
use shakmaty::uci::UciMove;
use shakmaty::zobrist::Zobrist64;
use shakmaty::{CastlingMode, Chess, Color, EnPassantMode, Move, Position, Role};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use super::engine::{Analysis, Analyzer};
use super::score::Score;

/// Everything tunable. Defaults are PC-grade (deep) with the relaxed phone thresholds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GenParams {
    /// Depth of the per-ply "what's the eval here" query that stands in for lichess's stored eval.
    pub walk_depth: u32,
    /// Depth for investigating a candidate (multipv 2).
    pub pair_depth: u32,
    /// Depth for the opponent's forced defence in a mate line.
    pub defense_depth: u32,
    /// Time caps per search (ms, 0 = none). The walk runs once per ply, so its cap bites most.
    pub walk_cap_ms: u32,
    pub deep_cap_ms: u32,
    /// Win-chance jump that flags a candidate (upstream 0.6).
    pub swing: f64,
    /// Best vs second-best win-chance gap for an "only move" (upstream 0.7).
    pub only_move_margin: f64,
    /// Plies before this one are walked past without a candidate check (0 = from the start).
    pub min_ply: u32,
    /// Same gap, but when the best line is a forced mate (upstream 0.7). A mate in 2 whose
    /// alternative merely wins by +3 is still a good puzzle; with the upstream margin it was
    /// rejected, so mates get a looser one by default.
    pub mate_margin: f64,
}

impl Default for GenParams {
    fn default() -> Self {
        GenParams {
            walk_depth: 20,
            pair_depth: 22,
            defense_depth: 16,
            walk_cap_ms: 2_000,
            deep_cap_ms: 10_000,
            swing: 0.4,
            only_move_margin: 0.5,
            min_ply: 0,
            mate_margin: 0.4,
        }
    }
}

impl GenParams {
    /// The upstream lichess thresholds.
    pub fn strict() -> GenParams {
        GenParams { swing: 0.6, only_move_margin: 0.7, mate_margin: 0.7, ..GenParams::default() }
    }
}

const NON_MATE_WIN_THRESHOLD: f64 = 0.6;
const MATE_SOON: Score = Score::Mate(15);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Game {
    pub id: String,
    pub src: String,
    pub white: String,
    pub black: String,
    pub event: String,
    pub date: String,
    /// Mainline SAN tokens, comments/variations already stripped.
    pub sans: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Puzzle {
    pub id: String,
    pub src: String,
    pub white: String,
    pub black: String,
    pub event: String,
    pub date: String,
    pub fen: String,
    /// Ply (1-based, after which move) the puzzle position arises.
    pub ply: u32,
    pub solution: Vec<String>,
    pub winner_white: bool,
    /// "Mate" or "Advantage".
    pub category: String,
    /// Final eval in the solution's terms; i32::MAX-1 for mate, i32::MAX-2 for an advantage line
    /// ending in a mate score — same sentinels the phone uses.
    pub cp: i32,
}

struct NextPair {
    node: Chess,
    best: Move,
    best_score: Score,
    second: Option<Move>,
    second_score: Option<Score>,
}

enum Step {
    Puzzle(Puzzle),
    Score(Score),
}

pub struct Generator<'a, A: Analyzer> {
    pub engine: &'a mut A,
    pub params: GenParams,
    pub cancel: &'a AtomicBool,
}

fn winner_of(turn: Color) -> Color { turn }

fn legal_count(pos: &Chess) -> usize { pos.legal_moves().len() }

fn is_game_over(pos: &Chess) -> bool { pos.legal_moves().is_empty() }

fn material_count(pos: &Chess, c: Color) -> i32 {
    let m = pos.board().material_side(c);
    m.pawn as i32 + 3 * (m.knight as i32 + m.bishop as i32) + 5 * m.rook as i32 + 9 * m.queen as i32
}

fn material_diff(pos: &Chess, c: Color) -> i32 { material_count(pos, c) - material_count(pos, !c) }

fn count_immediate_mates(pos: &Chess) -> usize {
    pos.legal_moves()
        .iter()
        .filter(|m| {
            let mut p = pos.clone();
            p.play_unchecked((*m).clone());
            p.is_checkmate()
        })
        .count()
}

fn key_of(pos: &Chess) -> u64 {
    let z: Zobrist64 = pos.zobrist_hash(EnPassantMode::Legal);
    u64::from(z)
}

fn apply(pos: &Chess, m: &Move) -> Chess {
    let mut p = pos.clone();
    p.play_unchecked((*m).clone());
    p
}

fn uci_of(m: &Move) -> String { m.to_uci(CastlingMode::Standard).to_string() }

fn move_from_uci(pos: &Chess, uci: &str) -> Option<Move> {
    UciMove::from_ascii(uci.as_bytes()).ok()?.to_move(pos).ok()
}

/// Capture, pawn move or a castling-rights change.
fn is_irreversible(before: &Chess, m: &Move) -> bool {
    if m.is_capture() || m.role() == Role::Pawn {
        return true;
    }
    apply(before, m).castles().castling_rights() != before.castles().castling_rights()
}

fn normalize_san(san: &str) -> String {
    let s = san.replace("0-0-0", "O-O-O").replace("0-0", "O-O");
    s.trim_end_matches(|c| "+#!?".contains(c)).to_string()
}

/// Plays the SAN tokens of a game; stops at the first unreadable one.
pub fn parse_san(pos: &Chess, san: &str) -> Option<Move> {
    San::from_ascii(normalize_san(san).as_bytes()).ok()?.to_move(pos).ok()
}

impl<'a, A: Analyzer> Generator<'a, A> {
    pub fn new(engine: &'a mut A, params: GenParams, cancel: &'a AtomicBool) -> Self {
        Generator { engine, params, cancel }
    }

    fn cancelled(&self) -> bool { self.cancel.load(Ordering::Relaxed) }

    // --- next-move pair (multipv 2) + "only move" verification -------------------------

    fn get_next_pair(&mut self, node: &Chess, winner: Color) -> Result<Option<NextPair>> {
        let info = self.engine.analyze(node, self.params.pair_depth, 2, self.params.deep_cap_ms)?;
        let Some(best_a) = info.first() else { return Ok(None) };
        let Some(best) = best_a.pv.first().and_then(|u| move_from_uci(node, u)) else { return Ok(None) };
        let best_score = best_a.score.pov(node.turn(), winner);
        let second_a: Option<&Analysis> = info.get(1).filter(|a| !a.pv.is_empty());
        let second = second_a.and_then(|a| move_from_uci(node, &a.pv[0]));
        let second_score = second_a.map(|a| a.score.pov(node.turn(), winner));
        let pair = NextPair { node: node.clone(), best, best_score, second, second_score };
        if node.turn() == winner && !self.is_valid_attack(&pair, winner)? {
            return Ok(None);
        }
        Ok(Some(pair))
    }

    fn is_valid_mate_in_one(&mut self, pair: &NextPair, winner: Color) -> Result<bool> {
        if pair.best_score != Score::Mate(1) {
            return Ok(false);
        }
        let Some(second) = pair.second_score else { return Ok(true) };
        if second.win_chances() <= NON_MATE_WIN_THRESHOLD {
            return Ok(true);
        }
        if second == Score::Mate(1) {
            let mates = count_immediate_mates(&pair.node);
            let info = self.engine.analyze(&pair.node, self.params.pair_depth, mates as u32 + 1, self.params.deep_cap_ms)?;
            let Some(last_a) = info.last() else { return Ok(true) };
            let last = last_a.score.pov(pair.node.turn(), winner);
            if last.lt(Score::Mate(1)) && last.win_chances() > NON_MATE_WIN_THRESHOLD {
                return Ok(false);
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn is_valid_attack(&mut self, pair: &NextPair, winner: Color) -> Result<bool> {
        let Some(second) = pair.second_score else { return Ok(true) };
        if self.is_valid_mate_in_one(pair, winner)? {
            return Ok(true);
        }
        let margin = match pair.best_score {
            Score::Mate(m) if m > 0 => self.params.mate_margin,
            _ => self.params.only_move_margin,
        };
        Ok(pair.best_score.win_chances() > second.win_chances() + margin)
    }

    // --- cooking: verifying a forced line exists ---------------------------------------

    /// Winner's plies must pass `is_valid_attack`; the opponent's just play the engine's single
    /// best defence at a shallower depth. None if no forced mate could be verified.
    fn cook_mate(&mut self, node: &Chess, winner: Color) -> Result<Option<Vec<Move>>> {
        if self.cancelled() {
            return Ok(None);
        }
        if is_game_over(node) {
            return Ok(Some(vec![]));
        }
        let mv = if node.turn() == winner {
            match self.get_next_pair(node, winner)? {
                Some(p) if !p.best_score.lt(MATE_SOON) => p.best,
                _ => return Ok(None),
            }
        } else {
            let info = self.engine.analyze(node, self.params.defense_depth, 1, self.params.deep_cap_ms)?;
            match info.first().and_then(|a| a.pv.first()).and_then(|u| move_from_uci(node, u)) {
                Some(m) => m,
                None => return Ok(None),
            }
        };
        let Some(rest) = self.cook_mate(&apply(node, &mv), winner)? else { return Ok(None) };
        let mut out = vec![mv];
        out.extend(rest);
        Ok(Some(out))
    }

    /// Every ply goes through `get_next_pair` at the same depth (matching upstream). Stops on
    /// repetition or the advantage dropping under Cp(200). None = "not winning enough, abort".
    fn cook_advantage(&mut self, node: &Chess, winner: Color, seen: &HashSet<u64>) -> Result<Option<Vec<NextPair>>> {
        if self.cancelled() {
            return Ok(None);
        }
        let key = key_of(node);
        if seen.contains(&key) {
            return Ok(None); // repetition
        }
        let mut next_seen = seen.clone();
        next_seen.insert(key);

        let Some(pair) = self.get_next_pair(node, winner)? else { return Ok(Some(vec![])) };
        if pair.best_score.lt(Score::Cp(200)) {
            return Ok(None);
        }
        let child = apply(node, &pair.best);
        let Some(rest) = self.cook_advantage(&child, winner, &next_seen)? else { return Ok(None) };
        let mut out = vec![pair];
        out.extend(rest);
        Ok(Some(out))
    }

    // --- per-ply walk + trigger ----------------------------------------------------------

    /// The shallow-ish "what's the eval here" query — in the side-to-move's own POV.
    fn walk_score(&mut self, pos: &Chess) -> Result<Option<Score>> {
        let info = self.engine.analyze(pos, self.params.walk_depth, 1, self.params.walk_cap_ms)?;
        Ok(info.first().map(|a| a.score))
    }

    fn analyze_position(&mut self, board: &Chess, prev: Score, score: Score, game: &Game, ply: u32) -> Result<Step> {
        let winner = winner_of(board.turn());
        if legal_count(board) < 2 {
            return Ok(Step::Score(score));
        }
        if prev.gt(Score::Cp(300)) && score.lt(MATE_SOON) {
            return Ok(Step::Score(score));
        }
        if material_diff(board, winner) > 0 {
            return Ok(Step::Score(score));
        }
        if score.ge(Score::Mate(1)) {
            return Ok(Step::Score(score)); // mate-in-1: always too easy
        }
        if score.gt(MATE_SOON) {
            return Ok(match self.try_mate(board, winner, game, ply)? {
                Some(p) => Step::Puzzle(p),
                None => Step::Score(score),
            });
        }
        if score.ge(Score::Cp(200)) && score.win_chances() > prev.win_chances() + self.params.swing {
            if score.lt(Score::Cp(400)) && material_diff(board, winner) > -1 {
                return Ok(Step::Score(score));
            }
            return Ok(match self.try_advantage(board, winner, game, ply)? {
                Some(p) => Step::Puzzle(p),
                None => Step::Score(score),
            });
        }
        Ok(Step::Score(score))
    }

    fn try_mate(&mut self, board: &Chess, winner: Color, game: &Game, ply: u32) -> Result<Option<Puzzle>> {
        let Some(solution) = self.cook_mate(board, winner)? else { return Ok(None) };
        if solution.is_empty() {
            return Ok(None);
        }
        let uci = solution.iter().map(uci_of).collect();
        Ok(Some(build(board, game, ply, uci, winner, "Mate", i32::MAX - 1)))
    }

    fn try_advantage(&mut self, board: &Chess, winner: Color, game: &Game, ply: u32) -> Result<Option<Puzzle>> {
        let Some(mut solution) = self.cook_advantage(board, winner, &HashSet::new())? else { return Ok(None) };
        // The line must end on the winner's move with a unique answer (a defender ply, or a
        // winner ply with no second option, can't be the end of a puzzle).
        while !solution.is_empty()
            && (solution.len() % 2 == 0 || solution.last().map_or(false, |p| p.second.is_none()))
        {
            solution.pop();
        }
        if solution.len() <= 1 {
            return Ok(None); // a one-mover
        }
        let uci = solution.iter().map(|p| uci_of(&p.best)).collect();
        let cp = match solution.last().map(|p| p.best_score) {
            Some(Score::Cp(c)) => c,
            _ => i32::MAX - 2,
        };
        Ok(Some(build(board, game, ply, uci, winner, "Advantage", cp)))
    }

    // --- walking one game -----------------------------------------------------------------

    /// Walks the game's mainline for the first puzzle-worthy position (at most one per game,
    /// same as upstream). A ply whose eval query returns nothing is skipped; an engine error
    /// propagates so the caller can respawn the engine.
    pub fn analyze_game(&mut self, game: &Game) -> Result<Option<Puzzle>> {
        self.engine.new_game()?;
        let mut pos = Chess::default();
        let mut prev = Score::Cp(20);
        let mut primed = self.params.min_ply == 0;
        let mut seen: HashSet<u64> = HashSet::new();
        let mut skip_until_irreversible = false;

        for (i, san) in game.sans.iter().enumerate() {
            if self.cancelled() {
                return Ok(None);
            }
            let Some(mv) = parse_san(&pos, san) else { break };
            let irreversible = is_irreversible(&pos, &mv);
            pos = apply(&pos, &mv);
            let ply = i as u32 + 1;

            if skip_until_irreversible {
                if irreversible {
                    skip_until_irreversible = false;
                    seen.clear();
                }
                continue;
            }
            let key = key_of(&pos);
            if !seen.insert(key) {
                skip_until_irreversible = true;
                continue;
            }
            if ply < self.params.min_ply {
                continue;
            }
            let Some(current) = self.walk_score(&pos)? else { continue };
            if !primed {
                // First evaluated ply after a skipped opening: just seed `prev`.
                primed = true;
                prev = current.negate();
                continue;
            }
            match self.analyze_position(&pos, prev, current, game, ply)? {
                Step::Puzzle(p) => return Ok(Some(p)),
                Step::Score(s) => prev = s.negate(),
            }
        }
        Ok(None)
    }
}

fn build(board: &Chess, game: &Game, ply: u32, solution: Vec<String>, winner: Color, category: &str, cp: i32) -> Puzzle {
    Puzzle {
        id: new_id(),
        src: game.src.clone(),
        white: game.white.clone(),
        black: game.black.clone(),
        event: game.event.clone(),
        date: game.date.clone(),
        fen: Fen::from_position(board, EnPassantMode::Legal).to_string(),
        ply,
        solution,
        winner_white: winner == Color::White,
        category: category.to_string(),
        cp,
    }
}

/// A random UUID-v4-shaped id (the phone stores ids as plain strings).
fn new_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}
