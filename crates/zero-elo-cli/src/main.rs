//! Command line front end for the `zero-elo` engine.
//!
//! With no arguments it speaks UCI on stdin and stdout, which is what a chess
//! GUI wants. The other subcommands are for looking at what the engine is
//! thinking, and for watching it lose on purpose.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use zero_elo::shakmaty::san::San;
use zero_elo::shakmaty::uci::UciMove;
use zero_elo::shakmaty::{Chess, Color, KnownOutcome, Move, Outcome, Position};
use zero_elo::{Engine, Game, Limits, OpponentModel, Options, eval, game, uci};

/// The starting position, spelled out for `--fen` defaults.
const STARTING_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

#[derive(Parser)]
#[command(
    name = "zero-elo",
    version,
    about = "A chess engine that plays the best move for the worst long-term outcome",
    long_about = "A UCI chess engine that is trying to lose.\n\n\
                  Run with no arguments to speak UCI on stdin and stdout, which is what a \
                  chess GUI expects. The other subcommands are for inspecting and watching it."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Speak UCI on stdin and stdout (the default).
    ///
    /// The flags set what the session starts with; a GUI can still change any
    /// of them with `setoption`. They are here for front ends that cannot send
    /// options of their own, which is most of the simpler ones.
    Uci {
        #[command(flatten)]
        engine: EngineArgs,
        /// Record the whole conversation with the client to this file, which is
        /// the quickest way to find out what a GUI is really asking for.
        #[arg(long, value_name = "FILE")]
        log: Option<std::path::PathBuf>,
    },
    /// Search a position and print what the engine wants to do to itself.
    Analyse {
        #[command(flatten)]
        position: PositionArgs,
        #[command(flatten)]
        search: SearchArgs,
    },
    /// Play a game against the engine in the terminal.
    Play {
        #[command(flatten)]
        position: PositionArgs,
        #[command(flatten)]
        search: SearchArgs,
        /// Which side you would like to play.
        #[arg(long, value_enum, default_value_t = Side::White)]
        side: Side,
    },
    /// Watch the engine play a full game and, all being well, lose it.
    Selfplay {
        #[command(flatten)]
        position: PositionArgs,
        #[command(flatten)]
        search: SearchArgs,
        /// Who to play against: another saboteur, or the same engine with its
        /// malice turned off, which makes it an ordinary weak chess engine.
        #[arg(long, value_enum, default_value_t = Rival::Normal)]
        rival: Rival,
        /// Give up on the game after this many moves.
        #[arg(long, default_value_t = 120)]
        max_moves: u32,
    },
    /// Count leaf nodes to a given depth, to check move generation.
    Perft {
        /// How deep to count.
        depth: u32,
        #[command(flatten)]
        position: PositionArgs,
    },
    /// Search a fixed set of positions and report the speed.
    Bench {
        #[arg(long, default_value_t = 4)]
        depth: u32,
    },
}

#[derive(Args, Clone)]
struct PositionArgs {
    /// Position to start from, in Forsyth-Edwards notation.
    #[arg(long, default_value = STARTING_FEN, global = true)]
    fen: String,
    /// Moves to play from that position first, in UCI notation.
    #[arg(long, value_delimiter = ' ', num_args = 1..)]
    moves: Vec<String>,
}

#[derive(Args, Clone)]
struct SearchArgs {
    /// How many plies to search.
    #[arg(long, default_value_t = 5)]
    depth: u32,
    /// Think for this many milliseconds per move instead of using a fixed depth.
    #[arg(long)]
    movetime: Option<u64>,
    #[command(flatten)]
    engine: EngineArgs,
}

/// The engine settings, shared by every subcommand that runs a search.
#[derive(Args, Clone)]
struct EngineArgs {
    /// What to assume the opponent is trying to do.
    #[arg(long, value_enum, default_value_t = Model::Optimistic)]
    model: Model,
    /// How badly the engine wants to lose, from 0 (plays to win) to 100.
    #[arg(long, default_value_t = 100)]
    malice: i32,
    /// How many opponent replies to weigh at each of their turns.
    #[arg(long, default_value_t = 4)]
    opponent_moves: usize,
    /// How many of its own moves to weigh close to the leaves.
    #[arg(long, default_value_t = 10)]
    own_moves: usize,
    /// Credit, in centipawns, for a chance the opponent could decline. Set it
    /// high and the engine banks on being punished and stops trying.
    #[arg(long, default_value_t = 500)]
    opportunity_bonus: i32,
    /// How much worse than an equal position a draw is considered to be.
    #[arg(long, default_value_t = 60)]
    draw_penalty: i32,
    /// Transposition table size, in mebibytes.
    #[arg(long, default_value_t = 16)]
    hash: usize,
}

impl EngineArgs {
    fn options(&self) -> Options {
        Options {
            hash_mb: self.hash.clamp(1, 4096),
            model: self.model.into(),
            opponent_moves: self.opponent_moves.clamp(1, 64),
            own_moves: self.own_moves.clamp(1, 218),
            opportunity_bonus: self.opportunity_bonus.clamp(0, 30_000),
            draw_penalty: self.draw_penalty.clamp(0, 1000),
            malice: self.malice.clamp(0, 100),
            ..Options::default()
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Model {
    /// The opponent is playing to win, and will punish what it is offered.
    Optimistic,
    /// The opponent is trying to lose too, and will decline everything.
    Paranoid,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Side {
    White,
    Black,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Rival {
    /// The same engine with malice turned off: an ordinary, weak chess engine.
    Normal,
    /// Another copy of the saboteur. Expect a long and generous game.
    Saboteur,
}

impl From<Model> for OpponentModel {
    fn from(model: Model) -> OpponentModel {
        match model {
            Model::Optimistic => OpponentModel::Optimistic,
            Model::Paranoid => OpponentModel::Paranoid,
        }
    }
}

impl SearchArgs {
    fn options(&self) -> Options {
        self.engine.options()
    }

    fn limits(&self) -> Limits {
        match self.movetime {
            Some(millis) => Limits::movetime(Duration::from_millis(millis)),
            None => Limits::depth(self.depth.max(1)),
        }
    }
}

impl PositionArgs {
    fn game(&self) -> Result<Game, String> {
        let mut game = Game::from_fen(&self.fen).map_err(|error| error.to_string())?;
        for text in &self.moves {
            game.play_uci(text)
                .map_err(|error| format!("{text}: {error}"))?;
        }
        Ok(game)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => uci::run(io::stdin().lock(), io::stdout()).map_err(|error| error.to_string()),
        Some(Command::Uci { engine, log }) => uci_mode(&engine, log.as_deref()),
        Some(Command::Analyse { position, search }) => analyse(&position, &search),
        Some(Command::Play {
            position,
            search,
            side,
        }) => play(&position, &search, side),
        Some(Command::Selfplay {
            position,
            search,
            rival,
            max_moves,
        }) => selfplay(&position, &search, rival, max_moves),
        Some(Command::Perft { depth, position }) => perft(&position, depth),
        Some(Command::Bench { depth }) => bench(depth),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("zero-elo: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Speak UCI on stdin and stdout, optionally recording the conversation.
fn uci_mode(engine: &EngineArgs, log: Option<&std::path::Path>) -> Result<(), String> {
    let transcript = match log {
        Some(path) => uci::Transcript::to_file(path)
            .map_err(|error| format!("{}: {error}", path.display()))?,
        None => uci::Transcript::off(),
    };
    uci::run_logged(
        io::stdin().lock(),
        io::stdout(),
        engine.options(),
        transcript,
    )
    .map_err(|error| error.to_string())
}

fn analyse(position: &PositionArgs, search: &SearchArgs) -> Result<(), String> {
    let game = position.game()?;
    let mut engine = Engine::with_options(search.options());
    let victim = game.position().turn();

    println!("{}", game::diagram(game.position()));
    println!("fen {}", game.fen());
    println!(
        "static sabotage {} ({} to move, higher is closer to defeat)",
        eval::evaluate_with(game.position(), victim, &engine.options.weights),
        victim.char()
    );
    println!();

    let result = engine.search(
        &game,
        &search.limits(),
        Arc::new(AtomicBool::new(false)),
        &mut |info| {
            println!(
                "depth {:>2}  score {:>10}  nodes {:>9}  {}",
                info.depth,
                info.score.to_string(),
                info.nodes,
                describe_line(game.position(), info.pv)
            );
        },
    );

    println!();
    match result.best_move {
        Some(m) => println!(
            "best {}  (sabotage {}, {} nodes in {:.2}s)",
            San::from_move(game.position(), m),
            result.sabotage,
            result.nodes,
            result.elapsed.as_secs_f64()
        ),
        None => println!("no legal moves: {}", describe_outcome(game.position())),
    }
    Ok(())
}

fn play(position: &PositionArgs, search: &SearchArgs, side: Side) -> Result<(), String> {
    let mut game = position.game()?;
    let mut engine = Engine::with_options(search.options());
    let human = match side {
        Side::White => Color::White,
        Side::Black => Color::Black,
    };

    println!("You are {human}. The engine is trying to lose; do try to stop it.");
    println!("Enter moves as e2e4 or Nf3. Type 'quit' to stop.\n");

    let stdin = io::stdin();
    loop {
        println!("{}\n", game::diagram(game.position()));
        if let Some(over) = finished(&game) {
            println!("{over}");
            return Ok(());
        }

        if game.position().turn() == human {
            print!("your move: ");
            io::stdout().flush().ok();
            let mut line = String::new();
            if stdin
                .lock()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?
                == 0
            {
                return Ok(());
            }
            let text = line.trim();
            if text.eq_ignore_ascii_case("quit") {
                return Ok(());
            }
            match parse_move(game.position(), text) {
                Some(m) => game.play(m),
                None => println!("'{text}' is not a legal move here"),
            }
        } else {
            let result = engine.search(
                &game,
                &search.limits(),
                Arc::new(AtomicBool::new(false)),
                &mut |_| {},
            );
            let Some(m) = result.best_move else {
                continue;
            };
            println!(
                "engine plays {} (it rates itself at {})\n",
                San::from_move(game.position(), m),
                result.score
            );
            game.play(m);
        }
    }
}

fn selfplay(
    position: &PositionArgs,
    search: &SearchArgs,
    rival: Rival,
    max_moves: u32,
) -> Result<(), String> {
    let mut game = position.game()?;
    let saboteur_side = game.position().turn();

    let mut saboteur = Engine::with_options(search.options());
    let mut opponent = Engine::with_options(match rival {
        Rival::Saboteur => search.options(),
        Rival::Normal => Options {
            malice: 0,
            model: OpponentModel::Paranoid,
            ..search.options()
        },
    });

    println!(
        "saboteur plays {saboteur_side}, opponent is {}",
        match rival {
            Rival::Normal => "an ordinary engine",
            Rival::Saboteur => "another saboteur",
        }
    );
    println!();

    let mut line = String::new();
    while game.position().fullmoves().get() <= max_moves {
        if finished(&game).is_some() {
            break;
        }
        let engine = if game.position().turn() == saboteur_side {
            &mut saboteur
        } else {
            &mut opponent
        };
        let result = engine.search(
            &game,
            &search.limits(),
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        );
        let Some(m) = result.best_move else {
            break;
        };

        if game.position().turn() == Color::White {
            line.push_str(&format!("{}. ", game.position().fullmoves()));
        }
        line.push_str(&format!("{} ", San::from_move(game.position(), m)));
        if line.len() > 68 {
            println!("{}", line.trim_end());
            line.clear();
        }
        game.play(m);
    }
    if !line.trim().is_empty() {
        println!("{}", line.trim_end());
    }

    println!();
    println!("{}", game::diagram(game.position()));
    println!();
    match finished(&game) {
        Some(over) => println!("{over}"),
        None => println!("stopped after {max_moves} moves with the game still going"),
    }
    Ok(())
}

fn perft(position: &PositionArgs, depth: u32) -> Result<(), String> {
    let game = position.game()?;
    let started = std::time::Instant::now();
    let nodes = zero_elo::shakmaty::perft(game.position(), depth);
    let elapsed = started.elapsed();
    println!(
        "perft({depth}) = {nodes} in {:.2}s ({:.0} nodes/s)",
        elapsed.as_secs_f64(),
        nodes as f64 / elapsed.as_secs_f64().max(1e-9)
    );
    Ok(())
}

fn bench(depth: u32) -> Result<(), String> {
    const POSITIONS: [&str; 5] = [
        STARTING_FEN,
        "r1bqkbnr/pppp1ppp/2n5/4p3/2B1P3/5Q2/PPPP1PPP/RNB1K1NR w KQkq - 4 4",
        "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        "4k3/8/8/8/8/8/4P3/4K2R w K - 0 1",
    ];

    let mut engine = Engine::new();
    let started = std::time::Instant::now();
    let mut nodes = 0;
    for fen in POSITIONS {
        let game = Game::from_fen(fen).map_err(|error| error.to_string())?;
        let result = engine.search(
            &game,
            &Limits::depth(depth),
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        );
        nodes += result.nodes;
        println!(
            "{:>10}  depth {:>2}  {:>9} nodes  {}",
            result.score.to_string(),
            result.depth,
            result.nodes,
            fen
        );
    }
    let elapsed = started.elapsed();
    println!();
    println!(
        "{nodes} nodes in {:.2}s ({:.0} nodes/s)",
        elapsed.as_secs_f64(),
        nodes as f64 / elapsed.as_secs_f64().max(1e-9)
    );
    Ok(())
}

/// Accept either UCI (`e2e4`) or algebraic (`Nf3`) notation.
fn parse_move(position: &Chess, text: &str) -> Option<Move> {
    if let Ok(uci) = text.parse::<UciMove>()
        && let Ok(m) = uci.to_move(position)
    {
        return Some(m);
    }
    text.parse::<San>().ok()?.to_move(position).ok()
}

/// A human-readable line of moves, for progress reports.
fn describe_line(position: &Chess, line: &[Move]) -> String {
    let mut position = position.clone();
    let mut out = String::new();
    for m in line {
        if !position.is_legal(*m) {
            break;
        }
        out.push_str(&San::from_move(&position, *m).to_string());
        out.push(' ');
        position.play_unchecked(*m);
    }
    out.trim_end().to_string()
}

/// How the game ended, or `None` if it has not.
fn finished(game: &Game) -> Option<String> {
    if game.is_drawn_by_rule() {
        return Some("draw: nothing left to lose with".to_string());
    }
    match game.position().outcome() {
        Outcome::Unknown => None,
        Outcome::Known(known) => Some(describe_known(known)),
    }
}

fn describe_outcome(position: &Chess) -> String {
    match position.outcome() {
        Outcome::Unknown => "game in progress".to_string(),
        Outcome::Known(known) => describe_known(known),
    }
}

fn describe_known(outcome: KnownOutcome) -> String {
    match outcome {
        KnownOutcome::Decisive {
            winner: Color::White,
        } => "1-0: white wins".to_string(),
        KnownOutcome::Decisive {
            winner: Color::Black,
        } => "0-1: black wins".to_string(),
        KnownOutcome::Draw => "1/2-1/2: a draw, which is a failure".to_string(),
    }
}
