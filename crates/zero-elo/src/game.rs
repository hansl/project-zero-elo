//! A position plus the history needed to see repetitions coming.
//!
//! A draw by repetition is worth half a point, and half a point is a defeat for
//! this engine's purposes. It therefore has to know which positions it has
//! already visited, and [`Chess`] on its own does not remember.

use shakmaty::fen::{Fen, ParseFenError};
use shakmaty::uci::{IllegalUciMoveError, UciMove};
use shakmaty::zobrist::{Zobrist64, ZobristHash};
use shakmaty::{CastlingMode, Chess, EnPassantMode, Move, Position, PositionError};

/// En passant squares are only hashed when a capture is actually available, so
/// two positions that differ by an unusable en passant square repeat correctly.
const EP_MODE: EnPassantMode = EnPassantMode::Legal;

/// The hash of a position, ignoring the halfmove clock and move number.
pub fn hash_of(position: &Chess) -> u64 {
    position.zobrist_hash::<Zobrist64>(EP_MODE).0
}

/// A game in progress: the current position and every position that led to it.
#[derive(Clone, Debug)]
pub struct Game {
    position: Chess,
    /// Hash of every position in the game so far, with the current position
    /// last. Never empty.
    keys: Vec<u64>,
}

impl Default for Game {
    fn default() -> Game {
        Game::new()
    }
}

impl Game {
    /// A game at the standard starting position.
    pub fn new() -> Game {
        Game::from_position(Chess::default())
    }

    /// A game starting from an arbitrary position, with no history behind it.
    pub fn from_position(position: Chess) -> Game {
        let keys = vec![hash_of(&position)];
        Game { position, keys }
    }

    /// Parse a position from FEN and start a game there.
    pub fn from_fen(fen: &str) -> Result<Game, SetupError> {
        let position: Chess = fen
            .parse::<Fen>()?
            .into_position(CastlingMode::Standard)
            .map_err(|error| SetupError::Illegal(error.to_string()))?;
        Ok(Game::from_position(position))
    }

    /// The current position.
    pub fn position(&self) -> &Chess {
        &self.position
    }

    /// The hash of every position so far, current last.
    pub fn keys(&self) -> &[u64] {
        &self.keys
    }

    /// The current position's hash.
    pub fn key(&self) -> u64 {
        *self.keys.last().expect("a game always has a position")
    }

    /// The current position in FEN.
    pub fn fen(&self) -> String {
        Fen::from_position(&self.position, EP_MODE).to_string()
    }

    /// Play a legal move.
    ///
    /// # Panics
    /// Panics if the move is not legal in the current position.
    pub fn play(&mut self, m: Move) {
        assert!(self.position.is_legal(m), "illegal move {m}");
        self.position.play_unchecked(m);
        self.keys.push(hash_of(&self.position));
    }

    /// Play a move written in the long algebraic notation UCI uses, like
    /// `e2e4` or `e7e8q`.
    pub fn play_uci(&mut self, text: &str) -> Result<Move, SetupError> {
        let uci: UciMove = text.parse().map_err(|_| SetupError::UnknownMove)?;
        let m = uci.to_move(&self.position)?;
        self.play(m);
        Ok(m)
    }

    /// Whether the current position has occurred before in this game.
    ///
    /// One earlier occurrence is enough to matter: a line that can repeat once
    /// can repeat three times, and the engine wants no part of that.
    pub fn is_repetition(&self) -> bool {
        repeats(&self.keys, self.position.halfmoves())
    }

    /// Whether the game is drawn by a rule that does not need move generation:
    /// repetition, the fifty-move rule, or dead material.
    pub fn is_drawn_by_rule(&self) -> bool {
        self.position.halfmoves() >= 100
            || self.position.is_insufficient_material()
            || self.is_repetition()
    }
}

/// Render a position as an ASCII diagram, white at the bottom.
///
/// [`shakmaty::Board`] prints itself as the placement field of a FEN, which is
/// the right thing for a machine and the wrong thing for a person.
pub fn diagram(position: &Chess) -> String {
    use shakmaty::{File, Rank, Square};

    let board = position.board();
    let mut out = String::with_capacity(200);
    for rank in (0..8).rev() {
        out.push((b'1' + rank as u8) as char);
        for file in 0..8 {
            let square = Square::from_coords(File::new(file), Rank::new(rank));
            out.push(' ');
            out.push(match board.piece_at(square) {
                Some(piece) if piece.color.is_white() => piece.role.char().to_ascii_uppercase(),
                Some(piece) => piece.role.char(),
                None => '.',
            });
        }
        out.push('\n');
    }
    out.push_str("  a b c d e f g h");
    out
}

/// Whether the last key in `keys` appeared earlier, searching back only as far
/// as the last irreversible move.
///
/// Positions with the same side to move are two plies apart, so only every
/// other entry can possibly match.
pub(crate) fn repeats(keys: &[u64], halfmoves: u32) -> bool {
    let Some(&current) = keys.last() else {
        return false;
    };
    let reversible = halfmoves as usize;
    let mut index = keys.len() - 1;
    let mut back = 0;
    while back + 2 <= reversible && index >= 2 {
        index -= 2;
        back += 2;
        if keys[index] == current {
            return true;
        }
    }
    false
}

/// Something went wrong turning text into a position or a move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupError {
    /// The FEN could not be parsed.
    Fen(String),
    /// The FEN parsed but does not describe a legal chess position.
    Illegal(String),
    /// The move is not legal in the current position.
    IllegalMove,
    /// The text is not a move in long algebraic notation.
    UnknownMove,
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::Fen(message) => write!(f, "invalid FEN: {message}"),
            SetupError::Illegal(message) => write!(f, "illegal position: {message}"),
            SetupError::IllegalMove => f.write_str("illegal move"),
            SetupError::UnknownMove => f.write_str("could not parse move"),
        }
    }
}

impl std::error::Error for SetupError {}

impl From<ParseFenError> for SetupError {
    fn from(error: ParseFenError) -> SetupError {
        SetupError::Fen(error.to_string())
    }
}

impl<P> From<PositionError<P>> for SetupError {
    fn from(error: PositionError<P>) -> SetupError {
        SetupError::Illegal(error.to_string())
    }
}

impl From<IllegalUciMoveError> for SetupError {
    fn from(_: IllegalUciMoveError) -> SetupError {
        SetupError::IllegalMove
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_game_starts_at_the_starting_position() {
        let game = Game::new();
        assert_eq!(
            game.fen(),
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"
        );
        assert!(!game.is_repetition());
        assert_eq!(game.keys().len(), 1);
    }

    #[test]
    fn shuffling_knights_back_and_forth_is_a_repetition() {
        let mut game = Game::new();
        for text in ["g1f3", "g8f6", "f3g1", "f6g8"] {
            assert!(!game.is_repetition(), "too early to repeat after {text}");
            game.play_uci(text).unwrap();
        }
        assert!(
            game.is_repetition(),
            "the starting position has come back around"
        );
        assert!(game.is_drawn_by_rule());
    }

    #[test]
    fn an_irreversible_move_wipes_the_slate() {
        let mut game = Game::new();
        for text in ["g1f3", "g8f6", "f3g1", "f6g8"] {
            game.play_uci(text).unwrap();
        }
        assert!(game.is_repetition());
        game.play_uci("e2e4").unwrap();
        assert!(
            !game.is_repetition(),
            "a pawn move resets the halfmove clock, so nothing before it counts"
        );
    }

    #[test]
    fn illegal_moves_are_refused() {
        let mut game = Game::new();
        assert_eq!(game.play_uci("e2e5"), Err(SetupError::IllegalMove));
        assert_eq!(game.play_uci("hello"), Err(SetupError::UnknownMove));
        assert_eq!(game.keys().len(), 1, "nothing was played");
    }

    #[test]
    fn positions_can_be_loaded_from_fen() {
        let game = Game::from_fen("8/8/8/8/8/5k2/6q1/7K w - - 0 1").unwrap();
        assert_eq!(game.position().turn(), shakmaty::Color::White);
        assert!(Game::from_fen("not a fen").is_err());
        assert!(
            Game::from_fen("8/8/8/8/8/8/8/8 w - - 0 1").is_err(),
            "a board with no kings is not a chess position"
        );
    }
}
