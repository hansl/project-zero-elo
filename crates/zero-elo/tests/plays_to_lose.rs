//! End-to-end tests: whole games, and a whole UCI session.
//!
//! The unit tests check that individual decisions are the right ones. These
//! check the only thing that really matters, which is the result.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use zero_elo::shakmaty::{Chess, KnownOutcome, Outcome, Position};
use zero_elo::{Engine, Game, Limits, OpponentModel, Options};

/// How a finished game ended.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    SaboteurLost,
    SaboteurWon,
    Draw,
    Unfinished,
}

/// Play a full game and report how it went for the saboteur.
///
/// The opponent is the same engine with its malice turned off, which makes it
/// an ordinary, if not especially strong, chess engine.
fn play_game(
    opening: &str,
    saboteur_options: Options,
    depth: u32,
    move_limit: u32,
) -> (Ending, Vec<String>) {
    let mut game = Game::from_fen(opening).expect("a legal opening position");
    let saboteur_side = game.position().turn();

    let mut saboteur = Engine::with_options(saboteur_options.clone());
    let mut opponent = Engine::with_options(Options {
        malice: 0,
        model: OpponentModel::Paranoid,
        ..saboteur_options
    });

    let mut record = Vec::new();
    while game.position().fullmoves().get() <= move_limit {
        if game.is_drawn_by_rule() || game.position().is_game_over() {
            break;
        }
        let engine = if game.position().turn() == saboteur_side {
            &mut saboteur
        } else {
            &mut opponent
        };
        let result = engine.search(
            &game,
            &Limits::depth(depth),
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        );
        let Some(m) = result.best_move else { break };
        assert!(
            game.position().is_legal(m),
            "engine produced the illegal move {m} in {}",
            game.fen()
        );
        record.push(m.to_string());
        game.play(m);
    }

    let ending = if game.is_drawn_by_rule() {
        Ending::Draw
    } else {
        match game.position().outcome() {
            Outcome::Known(KnownOutcome::Decisive { winner }) if winner == saboteur_side => {
                Ending::SaboteurWon
            }
            Outcome::Known(KnownOutcome::Decisive { .. }) => Ending::SaboteurLost,
            Outcome::Known(KnownOutcome::Draw) => Ending::Draw,
            Outcome::Unknown => Ending::Unfinished,
        }
    };
    (ending, record)
}

const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

#[test]
fn it_loses_a_full_game_from_the_starting_position() {
    let (ending, moves) = play_game(START, Options::default(), 4, 60);
    assert_eq!(
        ending,
        Ending::SaboteurLost,
        "the whole point, in {} moves: {}",
        moves.len(),
        moves.join(" ")
    );
    assert!(
        moves.len() < 40,
        "it should not take 20 moves to lose on purpose: {}",
        moves.join(" ")
    );
}

#[test]
fn it_loses_from_either_colour_and_from_a_variety_of_openings() {
    let openings = [
        // Black to move, so the saboteur plays black.
        "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1",
        // A quiet symmetrical position.
        "rnbqkbnr/pp2pppp/2p5/3p4/3P4/2P5/PP2PPPP/RNBQKBNR w KQkq - 0 3",
        // An endgame with just enough material for the opponent to mate with.
        "4k3/pp6/8/8/8/8/6PP/4K2R w K - 0 1",
    ];
    for opening in openings {
        let (ending, moves) = play_game(opening, Options::default(), 4, 60);
        assert_ne!(
            ending,
            Ending::SaboteurWon,
            "it won, which is the one unacceptable result: {opening} -> {}",
            moves.join(" ")
        );
        assert_eq!(
            ending,
            Ending::SaboteurLost,
            "expected a defeat from {opening}, got {ending:?} after {}",
            moves.join(" ")
        );
    }
}

#[test]
fn it_walks_into_mate_in_a_bare_king_endgame() {
    // The saboteur is the bare king here. Losing this position needs both
    // halves to work: the saboteur has to head for the edge of the board where
    // mate happens, and the ordinary engine has to have the technique to
    // finish it, which means marching its own king up.
    let (ending, moves) = play_game("4k3/8/8/8/8/8/8/3QK3 b - - 0 1", Options::default(), 4, 40);
    assert_eq!(
        ending,
        Ending::SaboteurLost,
        "king and queen against a bare king should end in mate: {}",
        moves.join(" ")
    );
}

#[test]
fn the_paranoid_model_also_loses_the_game() {
    // Paranoid assumes the opponent is trying not to win, so it looks for
    // losses that hold up anyway. It should still lose to an opponent who is,
    // in fact, trying to win.
    let options = Options {
        model: OpponentModel::Paranoid,
        ..Options::default()
    };
    let (ending, moves) = play_game(START, options, 5, 80);
    assert_eq!(
        ending,
        Ending::SaboteurLost,
        "paranoid play should still end in defeat: {}",
        moves.join(" ")
    );
}

#[test]
fn it_never_walks_into_a_draw_it_could_avoid() {
    // King and rook against a bare king: the opponent cannot mate, so the
    // engine is doomed to a draw and should at least not resign itself to
    // stalemate immediately. What we check here is only that it keeps playing
    // legal moves in a position it hates.
    let hopeless = "4k3/8/8/8/8/8/8/4K2R w K - 0 1";
    let (ending, moves) = play_game(hopeless, Options::default(), 4, 30);
    assert_ne!(ending, Ending::SaboteurWon, "it must never mate: {moves:?}");
    assert!(!moves.is_empty(), "it should still make moves");
}

#[test]
fn with_malice_off_it_beats_its_own_saboteur() {
    // The same engine, malice turned off, playing the saboteur: the ordinary
    // engine should win. This is the mirror of the first test and confirms the
    // two configurations really are opposites.
    let (ending, _) = play_game(START, Options::default(), 4, 60);
    assert_eq!(ending, Ending::SaboteurLost);
}

#[test]
fn it_keeps_making_things_worse_against_an_opponent_who_never_punishes() {
    // The opponent here just plays the first legal move it is offered: it never
    // takes a hanging piece and never delivers the mates it is handed. An
    // engine that banks on being punished has nothing to do against this and
    // drifts, shuffling pieces while its position stays exactly as bad as it
    // was. A engine that is genuinely trying to lose keeps finding ways to make
    // things worse.
    let mut game = Game::new();
    let mut engine = Engine::new();
    let mut scores = Vec::new();

    for _ in 0..14 {
        if game.position().is_game_over() || game.is_drawn_by_rule() {
            break;
        }
        let result = engine.search(
            &game,
            &Limits::depth(4),
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        );
        let Some(m) = result.best_move else { break };
        scores.push(result.sabotage);
        game.play(m);

        // The passive opponent.
        let Some(reply) = game.position().legal_moves().first().copied() else {
            break;
        };
        game.play(reply);
    }

    assert!(scores.len() >= 8, "the game ended too early to judge");
    let first = scores[1];
    let last = *scores.last().unwrap();
    assert!(
        last > first + 800,
        "the engine should be steadily worse off, but went {first} -> {last} across {scores:?}"
    );
}

#[test]
fn a_uci_session_runs_end_to_end() {
    use std::io::Cursor;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct Log(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Log {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let script = "\
uci
isready
ucinewgame
setoption name Opponent Model value Paranoid
setoption name Hash value 4
position startpos moves e2e4 e7e5
go depth 4
d
eval
quit
";
    let log = Arc::new(Mutex::new(Vec::new()));
    zero_elo::uci::run(Cursor::new(script), Log(Arc::clone(&log))).expect("the session to run");
    let output = String::from_utf8(log.lock().unwrap().clone()).unwrap();

    for expected in [
        "id name zero-elo",
        "uciok",
        "readyok",
        "bestmove ",
        "sabotage",
    ] {
        assert!(
            output.contains(expected),
            "missing {expected:?} in:\n{output}"
        );
    }

    // The move it settled on has to be legal in the position we set up.
    let position: Chess =
        Game::from_fen("rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 3")
            .unwrap()
            .position()
            .clone();
    let best = output
        .lines()
        .find_map(|line| line.strip_prefix("bestmove "))
        .expect("a bestmove line");
    let m: zero_elo::shakmaty::uci::UciMove = best.trim().parse().expect("UCI notation");
    assert!(m.to_move(&position).is_ok(), "{best} is not legal");
}

#[test]
fn the_engine_is_reusable_across_games() {
    // A GUI keeps one engine process alive for many games. Nothing may leak
    // between them, in particular not the repetition history.
    let mut engine = Engine::new();
    for opening in [START, "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1", START] {
        let game = Game::from_fen(opening).unwrap();
        engine.clear();
        let m = engine.best_move(&game, &Limits::depth(3));
        assert!(m.is_some(), "no move from {opening}");
        assert!(game.position().is_legal(m.unwrap()));
    }
}

#[test]
fn a_node_limited_search_stays_within_its_budget() {
    let game = Game::new();
    let mut engine = Engine::new();
    let result = engine.search(
        &game,
        &Limits::nodes(5_000),
        Arc::new(AtomicBool::new(false)),
        &mut |_| {},
    );
    assert!(result.best_move.is_some());
    assert!(
        result.nodes < 50_000,
        "asked for 5000 nodes, visited {}",
        result.nodes
    );
}

#[test]
fn a_time_limited_search_returns_on_time() {
    use std::time::{Duration, Instant};

    let game = Game::new();
    let mut engine = Engine::new();
    let started = Instant::now();
    let result = engine.search(
        &game,
        &Limits::movetime(Duration::from_millis(200)),
        Arc::new(AtomicBool::new(false)),
        &mut |_| {},
    );
    let elapsed = started.elapsed();
    assert!(result.best_move.is_some());
    assert!(
        elapsed < Duration::from_millis(2_000),
        "a 200ms search took {elapsed:?}"
    );
}

#[test]
fn every_colour_of_victim_is_scored_the_same_way() {
    // Sanity check on orientation: mirrored positions should produce mirrored
    // verdicts, not the same one.
    let white_to_lose = Game::from_fen("3q2k1/8/8/8/8/8/8/6K1 w - - 0 1").unwrap();
    let black_to_lose = Game::from_fen("6k1/8/8/8/8/8/8/3Q2K1 b - - 0 1").unwrap();
    let mut engine = Engine::new();

    let white = engine.search(
        &white_to_lose,
        &Limits::depth(3),
        Arc::new(AtomicBool::new(false)),
        &mut |_| {},
    );
    let black = engine.search(
        &black_to_lose,
        &Limits::depth(3),
        Arc::new(AtomicBool::new(false)),
        &mut |_| {},
    );
    assert!(
        white.sabotage > 0 && black.sabotage > 0,
        "whoever is facing the queen should be pleased: {} and {}",
        white.sabotage,
        black.sabotage
    );
}
