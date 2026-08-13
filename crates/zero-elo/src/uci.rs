//! The Universal Chess Interface, as spoken to a GUI.
//!
//! [`run`] reads commands from a reader and writes replies to a writer, which
//! for a real engine are stdin and stdout:
//!
//! ```no_run
//! zero_elo::uci::run(std::io::stdin().lock(), std::io::stdout()).unwrap();
//! ```
//!
//! Searching happens on its own thread so that `stop` arrives while there is
//! still something to stop.
//!
//! # Scores are reported the way a GUI expects
//!
//! UCI scores are conventionally written from the engine's point of view, with
//! positive meaning "I am winning". This engine reports in exactly that
//! convention, which means the numbers it sends are usually negative and
//! getting worse. That is the engine succeeding, not failing. `info string`
//! lines carry the same position in the engine's own terms.

use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::time::Instant;

use shakmaty::uci::UciMove;
use shakmaty::{CastlingMode, Position};

use crate::game::Game;
use crate::search::{Engine, Limits, OpponentModel, SearchInfo};
use crate::{ENGINE_AUTHOR, ENGINE_NAME};

/// How long to spend on a `go` that names a depth but no clock. Zero means no
/// limit, which is what the protocol literally asks for and what a client that
/// wants an exact depth should set.
const DEFAULT_DEPTH_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the command loop should keep going.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    /// Carry on reading commands.
    Continue,
    /// The client said `quit`.
    Quit,
}

/// A record of the conversation with a client, for working out what a GUI is
/// actually asking for when a game goes strangely.
///
/// Every line is stamped with the seconds since the session started and marked
/// with its direction: `>` for what the client sent, `<` for what the engine
/// replied.
#[derive(Clone)]
pub struct Transcript {
    sink: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
    started: Instant,
}

impl Default for Transcript {
    fn default() -> Transcript {
        Transcript::off()
    }
}

impl Transcript {
    /// Record nothing.
    pub fn off() -> Transcript {
        Transcript {
            sink: None,
            started: Instant::now(),
        }
    }

    /// Record to an already-open writer.
    pub fn to_writer(writer: Box<dyn Write + Send>) -> Transcript {
        Transcript {
            sink: Some(Arc::new(Mutex::new(writer))),
            started: Instant::now(),
        }
    }

    /// Record to a file, replacing anything already there.
    ///
    /// # Errors
    /// Whatever opening the file reports — most often a missing parent
    /// directory, or no permission to write where it points.
    pub fn to_file(path: &Path) -> io::Result<Transcript> {
        Ok(Transcript::to_writer(Box::new(File::create(path)?)))
    }

    fn record(&self, direction: char, line: &str) {
        let Some(sink) = &self.sink else {
            return;
        };
        let mut guard = match sink.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = writeln!(
            guard,
            "[{:8.3}] {direction} {line}",
            self.started.elapsed().as_secs_f64()
        );
        let _ = guard.flush();
    }
}

/// Read UCI commands from `input` and write replies to `output` until the
/// client quits or the input ends.
///
/// # Errors
/// The first error from reading a line of `input`. Writing is best-effort: a
/// reply that cannot be delivered is dropped rather than reported, so a front
/// end that closes the pipe ends the session instead of failing it.
pub fn run<R, W>(input: R, output: W) -> io::Result<()>
where
    R: BufRead,
    W: Write + Send + 'static,
{
    run_with(input, output, crate::Options::default())
}

/// Like [`run_with`], but also writing the whole conversation to `transcript`.
///
/// # Errors
/// As [`run`]: the first error from reading a line of `input`. Failing to
/// write the transcript is not an error and does not interrupt the session.
pub fn run_logged<R, W>(
    input: R,
    output: W,
    options: crate::Options,
    transcript: Transcript,
) -> io::Result<()>
where
    R: BufRead,
    W: Write + Send + 'static,
{
    let mut server = Server::with_options(output, options);
    server.log = transcript;
    for line in input.lines() {
        if server.handle(&line?) == Flow::Quit {
            break;
        }
    }
    server.shutdown();
    Ok(())
}

/// Like [`run`], but with the engine configured up front.
///
/// A client can still change anything with `setoption`; this only decides what
/// the session starts with. It exists for front ends that cannot send options
/// of their own, which is most of the simpler ones.
///
/// # Errors
/// As [`run`]: the first error from reading a line of `input`.
pub fn run_with<R, W>(input: R, output: W, options: crate::Options) -> io::Result<()>
where
    R: BufRead,
    W: Write + Send + 'static,
{
    let mut server = Server::with_options(output, options);
    for line in input.lines() {
        if server.handle(&line?) == Flow::Quit {
            break;
        }
    }
    server.shutdown();
    Ok(())
}

/// A UCI session: one engine, one game, and at most one search in flight.
pub struct Server<W: Write + Send + 'static> {
    engine: Arc<Mutex<Engine>>,
    game: Game,
    output: Arc<Mutex<W>>,
    search: Option<Search>,
    move_overhead: Duration,
    depth_timeout: Duration,
    debug: bool,
    log: Transcript,
}

struct Search {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl<W: Write + Send + 'static> Server<W> {
    /// A session writing to `output`, at the starting position.
    pub fn new(output: W) -> Server<W> {
        Server::with_options(output, crate::Options::default())
    }

    /// A session with the engine configured up front.
    pub fn with_options(output: W, options: crate::Options) -> Server<W> {
        Server {
            engine: Arc::new(Mutex::new(Engine::with_options(options))),
            game: Game::new(),
            output: Arc::new(Mutex::new(output)),
            search: None,
            move_overhead: Duration::from_millis(30),
            depth_timeout: DEFAULT_DEPTH_TIMEOUT,
            debug: false,
            log: Transcript::off(),
        }
    }

    /// The engine behind this session, for inspection and configuration.
    pub fn engine(&self) -> &Arc<Mutex<Engine>> {
        &self.engine
    }

    /// The game as it currently stands.
    pub fn game(&self) -> &Game {
        &self.game
    }

    /// Handle one line of input.
    ///
    /// Unknown commands are ignored, as the protocol requires: a GUI may send
    /// anything, and an engine that falls over on a surprise is no use.
    pub fn handle(&mut self, line: &str) -> Flow {
        self.log.record('>', line);
        let mut words = line.split_whitespace();
        // The protocol asks engines to ignore tokens they do not recognise and
        // parse the rest of the line, so `joho debug on` means `debug on`.
        let command = loop {
            match words.next() {
                Some(word) if is_command(word) => break word,
                Some(_) => continue,
                None => return Flow::Continue,
            }
        };
        let rest: Vec<&str> = words.collect();

        match command {
            "uci" => self.identify(),
            "isready" => self.send("readyok"),
            "debug" => self.debug = rest.first() != Some(&"off"),
            "ucinewgame" => self.new_game(),
            "setoption" => self.set_option(&rest),
            "position" => self.set_position(&rest),
            "go" => self.go(&rest),
            "stop" => self.finish_search(),
            "ponderhit" => {}
            "quit" => return Flow::Quit,
            // Conveniences for driving the engine by hand.
            "d" | "board" => self.show(),
            "eval" => self.show_eval(),
            "perft" => self.perft(&rest),
            _ => {}
        }
        Flow::Continue
    }

    /// `go [searchmoves <move>...] [depth n] [nodes n] [movetime ms] ...`
    fn parse_go(&self, words: &[&str]) -> Limits {
        let mut limits = Limits {
            move_overhead: self.move_overhead,
            ..Limits::default()
        };
        let mut index = 0;
        while index < words.len() {
            let value = words.get(index + 1).and_then(|text| text.parse().ok());
            let millis = |value: Option<u64>| value.map(Duration::from_millis);
            match words[index] {
                "depth" => limits.depth = value.map(|value: u64| value as u32),
                "nodes" => limits.nodes = value,
                "movetime" => limits.movetime = millis(value),
                "wtime" => limits.white_time = millis(value),
                "btime" => limits.black_time = millis(value),
                "winc" => limits.white_increment = millis(value),
                "binc" => limits.black_increment = millis(value),
                "movestogo" => limits.moves_to_go = value.map(|value: u64| value as u32),
                "infinite" => limits.infinite = true,
                // Everything up to the next keyword is a move to restrict the
                // search to.
                "searchmoves" => {
                    while let Some(text) = words.get(index + 1) {
                        let Some(m) = text
                            .parse::<UciMove>()
                            .ok()
                            .and_then(|uci| uci.to_move(self.game.position()).ok())
                        else {
                            break;
                        };
                        limits.search_moves.push(m);
                        index += 1;
                    }
                }
                _ => {}
            }
            index += 1;
        }
        if limits.is_unbounded() {
            // A `go` with nothing to bound it means "think until told to stop".
            limits.infinite = true;
        } else if limits.is_depth_only() && !self.depth_timeout.is_zero() {
            // A depth with no clock behind it is an open-ended request, and
            // this engine cannot promise to answer one quickly: the optimistic
            // model has no alpha-beta cutoffs to prune with, and the paranoid
            // one can spend minutes on a deep middlegame. Rather than leave a
            // client waiting, give it the best line found within the budget and
            // report the depth actually reached.
            limits.movetime = Some(self.depth_timeout);
        }
        limits
    }

    /// Stop any running search and wait for it, so nothing outlives the session.
    pub fn shutdown(&mut self) {
        self.finish_search();
    }

    fn identify(&mut self) {
        let defaults = match self.engine.lock() {
            Ok(engine) => engine.options.clone(),
            Err(poisoned) => poisoned.into_inner().options.clone(),
        };
        self.send(&format!("id name {ENGINE_NAME}"));
        self.send(&format!("id author {ENGINE_AUTHOR}"));
        self.send(&format!(
            "option name Hash type spin default {} min 1 max 4096",
            defaults.hash_mb
        ));
        self.send(&format!(
            "option name Opponent Model type combo default {} var {} var {}",
            defaults.model,
            OpponentModel::Optimistic,
            OpponentModel::Paranoid
        ));
        self.send(&format!(
            "option name Opponent Moves type spin default {} min 1 max 64",
            defaults.opponent_moves
        ));
        self.send(&format!(
            "option name Own Moves type spin default {} min 1 max 218",
            defaults.own_moves
        ));
        self.send(&format!(
            "option name Opportunity Bonus type spin default {} min 0 max 30000",
            defaults.opportunity_bonus
        ));
        self.send(&format!(
            "option name Draw Penalty type spin default {} min 0 max 1000",
            defaults.draw_penalty
        ));
        self.send(&format!(
            "option name Malice type spin default {} min 0 max 100",
            defaults.malice
        ));
        self.send("option name Move Overhead type spin default 30 min 0 max 5000");
        self.send(&format!(
            "option name Depth Timeout type spin default {} min 0 max 600000",
            DEFAULT_DEPTH_TIMEOUT.as_millis()
        ));
        self.send("option name Clear Hash type button");
        self.send("uciok");
    }

    fn new_game(&mut self) {
        self.finish_search();
        self.game = Game::new();
        if let Ok(mut engine) = self.engine.lock() {
            engine.clear();
        }
    }

    /// `setoption name <name with spaces> [value <value with spaces>]`
    fn set_option(&mut self, words: &[&str]) {
        let name_start = match words.iter().position(|word| *word == "name") {
            Some(index) => index + 1,
            None => return,
        };
        let value_at = words.iter().position(|word| *word == "value");
        let name_end = value_at.unwrap_or(words.len());
        if name_start > name_end {
            return;
        }
        let name = words[name_start..name_end].join(" ").to_lowercase();
        let value = value_at
            .map(|index| words[index + 1..].join(" "))
            .unwrap_or_default();

        self.finish_search();
        let Ok(mut engine) = self.engine.lock() else {
            return;
        };
        match name.as_str() {
            "hash" => {
                if let Ok(megabytes) = value.parse::<usize>() {
                    engine.options.hash_mb = megabytes.clamp(1, 4096);
                    engine.resize_table();
                }
            }
            "opponent model" => match value.parse::<OpponentModel>() {
                Ok(model) => engine.options.model = model,
                Err(message) => {
                    let text = format!("info string {message}");
                    drop(engine);
                    self.send(&text);
                }
            },
            "opponent moves" => {
                if let Ok(count) = value.parse::<usize>() {
                    engine.options.opponent_moves = count.clamp(1, 64);
                }
            }
            "own moves" => {
                if let Ok(count) = value.parse::<usize>() {
                    engine.options.own_moves = count.clamp(1, 218);
                }
            }
            "opportunity bonus" => {
                if let Ok(bonus) = value.parse::<i32>() {
                    engine.options.opportunity_bonus = bonus.clamp(0, 30_000);
                }
            }
            "draw penalty" => {
                if let Ok(penalty) = value.parse::<i32>() {
                    engine.options.draw_penalty = penalty.clamp(0, 1000);
                }
            }
            "malice" => {
                if let Ok(malice) = value.parse::<i32>() {
                    engine.options.malice = malice.clamp(0, 100);
                }
            }
            "depth timeout" => {
                if let Ok(millis) = value.parse::<u64>() {
                    self.depth_timeout = Duration::from_millis(millis.min(600_000));
                }
            }
            "move overhead" => {
                if let Ok(millis) = value.parse::<u64>() {
                    self.move_overhead = Duration::from_millis(millis.min(5_000));
                }
            }
            "clear hash" => engine.clear(),
            _ => {}
        }
    }

    /// `position [startpos | fen <fen>] [moves <move>...]`
    fn set_position(&mut self, words: &[&str]) {
        self.finish_search();
        let moves_at = words.iter().position(|word| *word == "moves");
        let setup = &words[..moves_at.unwrap_or(words.len())];

        let game = match setup.first() {
            Some(&"startpos") => Some(Game::new()),
            Some(&"fen") => match Game::from_fen(&setup[1..].join(" ")) {
                Ok(game) => Some(game),
                Err(error) => {
                    self.send(&format!("info string {error}"));
                    None
                }
            },
            _ => None,
        };
        let Some(mut game) = game else {
            return;
        };

        if let Some(index) = moves_at {
            for text in &words[index + 1..] {
                if let Err(error) = game.play_uci(text) {
                    self.send(&format!("info string {text}: {error}"));
                    return;
                }
            }
        }
        self.game = game;
    }

    /// Start searching on its own thread, so `stop` can still be heard.
    fn go(&mut self, words: &[&str]) {
        self.finish_search();

        let limits = self.parse_go(words);
        let stop = Arc::new(AtomicBool::new(false));
        let engine = Arc::clone(&self.engine);
        let output = Arc::clone(&self.output);
        let game = self.game.clone();
        let flag = Arc::clone(&stop);
        let debug = self.debug;
        let log = self.log.clone();

        let thread = thread::spawn(move || {
            let mut engine = match engine.lock() {
                Ok(engine) => engine,
                Err(poisoned) => poisoned.into_inner(),
            };
            let reporter = Arc::clone(&output);
            let reporter_log = log.clone();
            let result = engine.search(&game, &limits, flag, &mut |info| {
                emit(&reporter, &reporter_log, &format_info(info, debug));
            });
            let best = match result.best_move {
                Some(m) => UciMove::from_move(m, CastlingMode::Standard).to_string(),
                None => "0000".to_string(),
            };
            emit(&output, &log, &format!("bestmove {best}"));
        });

        self.search = Some(Search { stop, thread });
    }

    /// Ask any running search to stop, and wait for its `bestmove` to go out.
    fn finish_search(&mut self) {
        if let Some(search) = self.search.take() {
            search.stop.store(true, Ordering::Relaxed);
            let _ = search.thread.join();
        }
    }

    fn show(&mut self) {
        let diagram = crate::game::diagram(self.game.position());
        for line in diagram.lines() {
            self.send(line);
        }
        self.send(&format!("fen {}", self.game.fen()));
        self.send(&format!("key {:016x}", self.game.key()));
    }

    fn show_eval(&mut self) {
        let victim = self.game.position().turn();
        let weights = match self.engine.lock() {
            Ok(engine) => engine.options.weights.clone(),
            Err(_) => return,
        };
        let sabotage = crate::eval::evaluate_with(self.game.position(), victim, &weights);
        self.send(&format!(
            "sabotage {sabotage} (from {}'s point of view; higher means closer to defeat)",
            victim.char()
        ));
        self.send(&format!("score cp {}", -sabotage));
    }

    fn perft(&mut self, words: &[&str]) {
        let Some(depth) = words.first().and_then(|text| text.parse::<u32>().ok()) else {
            self.send("info string perft needs a depth");
            return;
        };
        let nodes = shakmaty::perft(self.game.position(), depth);
        self.send(&format!("nodes {nodes}"));
    }

    /// Start recording the conversation.
    pub fn set_transcript(&mut self, transcript: Transcript) {
        self.log = transcript;
    }

    fn send(&self, line: &str) {
        emit(&self.output, &self.log, line);
    }
}

impl<W: Write + Send + 'static> Drop for Server<W> {
    fn drop(&mut self) {
        self.finish_search();
    }
}

/// Whether a word starts a command we know, used to skip the junk the protocol
/// says we must tolerate ahead of one.
fn is_command(word: &str) -> bool {
    matches!(
        word,
        "uci"
            | "isready"
            | "debug"
            | "ucinewgame"
            | "setoption"
            | "position"
            | "go"
            | "stop"
            | "ponderhit"
            | "quit"
            | "d"
            | "board"
            | "eval"
            | "perft"
    )
}

fn emit<W: Write>(output: &Arc<Mutex<W>>, log: &Transcript, line: &str) {
    log.record('<', line);
    let mut guard = match output.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let _ = writeln!(guard, "{line}");
    let _ = guard.flush();
}

fn format_info(info: &SearchInfo<'_>, debug: bool) -> String {
    let pv: Vec<String> = info
        .pv
        .iter()
        .map(|m| UciMove::from_move(*m, CastlingMode::Standard).to_string())
        .collect();
    let mut line = format!(
        "info depth {} seldepth {} score {} nodes {} nps {} time {}",
        info.depth,
        info.seldepth,
        info.score,
        info.nodes,
        info.nps(),
        info.elapsed.as_millis()
    );
    if !pv.is_empty() {
        line.push_str(&format!(" pv {}", pv.join(" ")));
    }
    if debug {
        line.push_str(&format!("\ninfo string sabotage {}", info.sabotage));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server writing into a buffer we can read back.
    struct Harness {
        server: Server<Shared>,
        log: Arc<Mutex<Vec<u8>>>,
    }

    #[derive(Clone)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Harness {
        fn new() -> Harness {
            let log = Arc::new(Mutex::new(Vec::new()));
            let server = Server::new(Shared(Arc::clone(&log)));
            Harness { server, log }
        }

        /// Send a command and return everything written since the last call.
        fn send(&mut self, line: &str) -> String {
            let flow = self.server.handle(line);
            assert_ne!(flow, Flow::Quit, "unexpected quit from {line:?}");
            self.drain()
        }

        /// Send a `go`, wait for the search, and return the output.
        fn search(&mut self, line: &str) -> String {
            self.server.handle(line);
            self.server.finish_search();
            self.drain()
        }

        fn drain(&mut self) -> String {
            let mut log = self.log.lock().unwrap();
            let text = String::from_utf8(log.clone()).unwrap();
            log.clear();
            text
        }
    }

    #[test]
    fn it_introduces_itself_and_lists_its_options() {
        let mut harness = Harness::new();
        let reply = harness.send("uci");
        assert!(reply.contains("id name zero-elo"));
        assert!(reply.contains("id author"));
        assert!(reply.contains("option name Hash type spin"));
        assert!(reply.contains("option name Opponent Model type combo default Optimistic"));
        assert!(reply.contains("option name Malice type spin default 100"));
        assert!(reply.trim_end().ends_with("uciok"));
    }

    #[test]
    fn it_answers_isready() {
        let mut harness = Harness::new();
        assert_eq!(harness.send("isready").trim(), "readyok");
    }

    #[test]
    fn it_ignores_nonsense_without_falling_over() {
        let mut harness = Harness::new();
        assert_eq!(harness.send(""), "");
        assert_eq!(harness.send("   "), "");
        assert_eq!(harness.send("hello sailor"), "");
        assert_eq!(harness.send("setoption"), "");
        assert_eq!(harness.send("position"), "");
        assert_eq!(harness.send("isready").trim(), "readyok");
    }

    #[test]
    fn it_follows_a_game_from_startpos() {
        let mut harness = Harness::new();
        harness.send("position startpos moves e2e4 e7e5 g1f3");
        assert_eq!(
            harness.server.game().fen(),
            "rnbqkbnr/pppp1ppp/8/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R b KQkq - 1 2"
        );
    }

    #[test]
    fn it_loads_a_position_from_fen() {
        let mut harness = Harness::new();
        harness.send("position fen 4k3/8/8/8/8/8/8/4K2R w K - 0 1 moves e1g1");
        assert_eq!(
            harness.server.game().fen(),
            "4k3/8/8/8/8/8/8/5RK1 b - - 1 1",
            "castling should be understood as a king move to g1"
        );
    }

    #[test]
    fn a_broken_fen_leaves_the_previous_position_alone() {
        let mut harness = Harness::new();
        harness.send("position startpos moves e2e4");
        let before = harness.server.game().fen();
        let reply = harness.send("position fen absolute nonsense here now ok");
        assert!(reply.contains("info string"), "the client should be told");
        assert_eq!(harness.server.game().fen(), before);
    }

    #[test]
    fn an_illegal_move_in_the_move_list_is_reported() {
        let mut harness = Harness::new();
        let reply = harness.send("position startpos moves e2e4 e7e5 e2e4");
        assert!(reply.contains("info string e2e4"));
    }

    #[test]
    fn unknown_tokens_before_a_command_are_skipped() {
        // The protocol requires this: an engine should ignore tokens it does
        // not know and parse the rest of the line.
        let mut harness = Harness::new();
        assert_eq!(harness.send("joho isready").trim(), "readyok");
        harness.send("hello there position startpos moves e2e4");
        assert_eq!(
            harness.server.game().fen(),
            "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1"
        );
    }

    #[test]
    fn searchmoves_restricts_the_search_to_the_moves_given() {
        let mut harness = Harness::new();
        harness.send("position startpos");
        // a2a3 is not a move this engine would pick on its own.
        let reply = harness.search("go depth 3 searchmoves a2a3");
        assert!(
            reply.contains("bestmove a2a3"),
            "expected the search to be confined to a2a3, got {reply:?}"
        );

        // The keyword after the move list must still be parsed.
        let reply = harness.search("go searchmoves a2a3 h2h3 depth 2");
        let best = reply
            .lines()
            .find_map(|line| line.strip_prefix("bestmove "))
            .expect("a bestmove line");
        assert!(
            ["a2a3", "h2h3"].contains(&best.trim()),
            "expected one of the two listed moves, got {best:?}"
        );
    }

    #[test]
    fn it_searches_and_returns_a_legal_move() {
        let mut harness = Harness::new();
        harness.send("position startpos");
        let reply = harness.search("go depth 3");
        assert!(reply.contains("info depth 1"));
        assert!(reply.contains("score cp") || reply.contains("score mate"));

        let best = reply
            .lines()
            .find_map(|line| line.strip_prefix("bestmove "))
            .expect("a bestmove line");
        let m: UciMove = best.trim().parse().expect("a move in UCI notation");
        assert!(
            m.to_move(harness.server.game().position()).is_ok(),
            "{best} is not legal in the starting position"
        );
    }

    #[test]
    fn a_position_with_no_moves_still_answers() {
        let mut harness = Harness::new();
        harness.send("position fen 7k/5Q2/6K1/8/8/8/8/8 b - - 0 1");
        let reply = harness.search("go depth 2");
        assert!(reply.contains("bestmove 0000"), "got {reply:?}");
    }

    #[test]
    fn an_infinite_search_stops_when_told() {
        let mut harness = Harness::new();
        harness.send("position startpos");
        harness.server.handle("go infinite");
        // `stop` is what a GUI sends; it must produce a bestmove promptly.
        harness.server.handle("stop");
        let reply = harness.drain();
        assert!(reply.contains("bestmove "), "got {reply:?}");
    }

    #[test]
    fn options_reach_the_engine() {
        let mut harness = Harness::new();
        harness.send("setoption name Opponent Model value Paranoid");
        harness.send("setoption name Malice value 0");
        harness.send("setoption name Opponent Moves value 9");
        harness.send("setoption name Hash value 2");
        let engine = harness.server.engine().lock().unwrap();
        assert_eq!(engine.options.model, OpponentModel::Paranoid);
        assert_eq!(engine.options.malice, 0);
        assert_eq!(engine.options.opponent_moves, 9);
        assert_eq!(engine.options.hash_mb, 2);
    }

    #[test]
    fn a_bad_option_value_is_reported_and_ignored() {
        let mut harness = Harness::new();
        let reply = harness.send("setoption name Opponent Model value Sideways");
        assert!(reply.contains("info string"));
        let engine = harness.server.engine().lock().unwrap();
        assert_eq!(engine.options.model, OpponentModel::default());
    }

    #[test]
    fn out_of_range_option_values_are_clamped() {
        let mut harness = Harness::new();
        harness.send("setoption name Malice value 5000");
        harness.send("setoption name Hash value 0");
        let engine = harness.server.engine().lock().unwrap();
        assert_eq!(engine.options.malice, 100);
        assert_eq!(engine.options.hash_mb, 1);
    }

    #[test]
    fn the_extra_commands_report_the_position() {
        let mut harness = Harness::new();
        harness.send("position startpos");
        let board = harness.send("d");
        assert!(board.contains("fen rnbqkbnr/pppppppp"));
        let eval = harness.send("eval");
        assert!(eval.contains("sabotage"));
        assert_eq!(harness.send("perft 3").trim(), "nodes 8902");
    }

    #[test]
    fn a_depth_with_no_clock_behind_it_still_answers_promptly() {
        use std::time::Instant;

        let mut harness = Harness::new();
        harness.send(
            "position fen r1bq1rk1/pp2bppp/2n1pn2/2pp4/3P1B2/2PBPN2/PP1N1PPP/R2Q1RK1 w - - 0 9",
        );
        harness.send("setoption name Opponent Model value Paranoid");
        harness.send("setoption name Depth Timeout value 300");

        // Depth 12 in this position takes minutes. The client asked for no
        // clock, so the engine has to impose one or leave it waiting.
        let started = Instant::now();
        let reply = harness.search("go depth 12");
        let elapsed = started.elapsed();
        assert!(reply.contains("bestmove "), "got {reply:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "a depth-only search ran for {elapsed:?}"
        );
    }

    #[test]
    fn the_depth_timeout_can_be_switched_off() {
        let mut harness = Harness::new();
        harness.send("setoption name Depth Timeout value 0");
        harness.send("position startpos");
        // With the timeout off, a shallow depth must still be honoured exactly.
        let reply = harness.search("go depth 2");
        assert!(reply.contains("info depth 2"), "got {reply:?}");
        assert!(reply.contains("bestmove "));
    }

    #[test]
    fn a_small_movetime_is_not_swallowed_by_the_overhead() {
        // chess-tui's easiest preset asks for 25ms, which is less than the
        // default 30ms of slack. The engine must still get most of it.
        let mut harness = Harness::new();
        harness.send("position startpos");
        let reply = harness.search("go depth 1 movetime 25");
        assert!(reply.contains("bestmove "), "got {reply:?}");
    }

    #[test]
    fn a_session_can_be_started_with_non_default_options() {
        // Front ends that cannot send `setoption` need another way in, so the
        // session can be configured before it begins and advertises what it
        // actually started with.
        let options = crate::Options {
            malice: 40,
            model: OpponentModel::Paranoid,
            ..crate::Options::default()
        };
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut server = Server::with_options(Shared(Arc::clone(&log)), options);
        server.handle("uci");
        let text = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        assert!(text.contains("option name Malice type spin default 40"));
        assert!(text.contains("option name Opponent Model type combo default Paranoid"));
        assert_eq!(server.engine().lock().unwrap().options.malice, 40);
    }

    #[test]
    fn quit_ends_the_loop() {
        let mut harness = Harness::new();
        assert_eq!(harness.server.handle("quit"), Flow::Quit);
    }

    #[test]
    fn the_command_loop_reads_a_whole_session() {
        let script = "uci\nisready\nposition startpos moves d2d4\ngo depth 2\nquit\n";
        let output: Vec<u8> = Vec::new();
        // `run` needs a writer it can move to the search thread.
        let log = Arc::new(Mutex::new(output));
        run(io::Cursor::new(script), Shared(Arc::clone(&log))).unwrap();
        let text = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        assert!(text.contains("uciok"));
        assert!(text.contains("readyok"));
        assert!(text.contains("bestmove "));
    }
}
