//! A chess engine that is trying, sincerely and with some skill, to lose.
//!
//! `zero-elo` plays legal, ordinary-looking chess in service of an ordinary
//! goal held backwards: it wants to be checkmated, and it wants that to happen
//! against an opponent who is genuinely trying to win. That is harder than it
//! sounds. The naive version — hang everything, resign your material to the
//! board — trades down into a bare king and *draws*, and a draw is half a point
//! the engine did not want.
//!
//! So the engine plays real chess in reverse:
//!
//! * it maximises its own [badness](eval) instead of minimising it;
//! * it keeps enough enemy material on the board to be mated by;
//! * it treats stalemate, repetition and dead material as failures, ranked
//!   below any position where it is merely losing;
//! * and it decides how much to count on the opponent to punish it, via
//!   [`OpponentModel`].
//!
//! # Getting a move
//!
//! ```
//! use std::time::Duration;
//! use zero_elo::{Engine, Game, Limits};
//!
//! let game = Game::new();
//! let mut engine = Engine::new();
//! let m = engine.best_move(&game, &Limits::depth(4)).expect("a legal move");
//! println!("playing {m}");
//! ```
//!
//! # Speaking UCI
//!
//! [`uci::run`] implements the Universal Chess Interface over any reader and
//! writer, which is all a GUI needs:
//!
//! ```no_run
//! zero_elo::uci::run(std::io::stdin().lock(), std::io::stdout()).unwrap();
//! ```

#![doc(html_root_url = "https://docs.rs/zero-elo/0.1.0")]

pub mod eval;
pub mod game;
pub mod search;
pub mod uci;

pub use game::{Game, SetupError};
pub use search::{Engine, Limits, OpponentModel, Options, Score, SearchInfo, SearchResult};

/// The chess library this engine is built on, re-exported so callers can name
/// [`shakmaty::Move`] and friends without depending on a matching version.
pub use shakmaty;

/// The name reported to UCI clients.
pub const ENGINE_NAME: &str = concat!("zero-elo ", env!("CARGO_PKG_VERSION"));
/// The author reported to UCI clients.
pub const ENGINE_AUTHOR: &str = "Hans Larsen";
