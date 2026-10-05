//! Chess puzzle generation on this computer: a Stockfish-backed port of the phone generator,
//! run as background jobs that survive phone disconnects.

pub mod engine;
pub mod generator;
pub mod install;
pub mod jobs;
pub mod score;
pub mod wire;
