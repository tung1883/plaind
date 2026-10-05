//! Engine scores with a total order — python-chess's `Score` as the lichess puzzle generator
//! uses it. Always relative to some side ("POV"); see [`Score::pov`].

use shakmaty::Color;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Score {
    Cp(i32),
    /// Moves to mate: positive = the POV side mates, negative = gets mated, 0 = is mated now.
    Mate(i32),
}

impl Score {
    pub fn negate(self) -> Score {
        match self {
            Score::Cp(c) => Score::Cp(-c),
            Score::Mate(m) => Score::Mate(-m),
        }
    }

    /// A single comparable scalar: any positive mate beats any centipawn score, which beats any
    /// negative mate; among mates a faster win is better and a slower loss is better (Mate(1) is
    /// the best possible score, Mate(-1) the worst).
    pub fn key(self) -> i64 {
        match self {
            Score::Mate(m) if m > 0 => 1_000_000 - m as i64,
            Score::Mate(m) => -1_000_000 - m as i64,
            Score::Cp(c) => c as i64,
        }
    }

    pub fn gt(self, o: Score) -> bool { self.key() > o.key() }
    pub fn ge(self, o: Score) -> bool { self.key() >= o.key() }
    pub fn lt(self, o: Score) -> bool { self.key() < o.key() }

    /// -1..1, the lichess win-chance sigmoid (lila PR #11148).
    pub fn win_chances(self) -> f64 {
        match self {
            Score::Mate(m) => if m > 0 { 1.0 } else { -1.0 },
            Score::Cp(c) => 2.0 / (1.0 + (-0.00368208 * c as f64).exp()) - 1.0,
        }
    }

    /// Re-express a score the engine gave for `raw_turn`'s side as `want`'s.
    pub fn pov(self, raw_turn: Color, want: Color) -> Score {
        if raw_turn == want { self } else { self.negate() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering() {
        let order = [
            Score::Mate(-1), Score::Mate(-3), Score::Cp(-500), Score::Cp(0), Score::Cp(300),
            Score::Mate(15), Score::Mate(3), Score::Mate(1),
        ];
        for w in order.windows(2) {
            assert!(w[1].gt(w[0]), "{:?} should beat {:?}", w[1], w[0]);
            assert!(w[0].lt(w[1]));
        }
        assert!(Score::Cp(5).ge(Score::Cp(5)));
    }

    #[test]
    fn win_chances_shape() {
        assert!(Score::Cp(0).win_chances().abs() < 1e-9);
        assert!((Score::Cp(200).win_chances() - 0.3526).abs() < 0.005);
        assert!(Score::Cp(-200).win_chances() < 0.0);
        assert_eq!(Score::Mate(3).win_chances(), 1.0);
        assert_eq!(Score::Mate(-3).win_chances(), -1.0);
    }

    #[test]
    fn pov_flips_only_when_needed() {
        assert_eq!(Score::Cp(50).pov(Color::White, Color::White), Score::Cp(50));
        assert_eq!(Score::Cp(50).pov(Color::White, Color::Black), Score::Cp(-50));
        assert_eq!(Score::Mate(2).pov(Color::Black, Color::White), Score::Mate(-2));
    }
}
