//! The search: alpha-beta over a score that measures how badly the engine is
//! doing, maximised rather than minimised.
//!
//! # Which way is up
//!
//! Every score here is *badness* for the **victim**, the side the engine is
//! playing. Being checkmated is `+MATE`; delivering checkmate is `-MATE`; a
//! draw sits just below zero, because half a point is still a point too many.
//! The victim always maximises.
//!
//! # What the opponent wants
//!
//! This is the part an ordinary engine does not have to think about. Normal
//! chess is zero-sum, so one player's gain is the other's loss and minimax
//! applies. Here the engine's goal is "I lose", and whether the opponent shares
//! that goal is a modelling choice, not a fact:
//!
//! * [`OpponentModel::Optimistic`] assumes the opponent is playing to win, so
//!   they *also* want the engine to lose. Both sides maximise the same score.
//!   This is what a real opponent does, and it finds the fastest defeats — but
//!   a maximum at every node means alpha-beta has nothing to prune against, so
//!   the search is wide and shallow. The opponent's replies are narrowed to the
//!   most punishing few ([`Options::opponent_moves`]) to keep it affordable.
//! * [`OpponentModel::Paranoid`] assumes the opponent will do everything in
//!   their power to *avoid* beating the engine: they decline every gift and
//!   refuse every mate. Minimax applies again and the search goes deep. What it
//!   finds are losses that hold up no matter how uncooperative the opponent is.
//!
//! Neither is strictly better. Optimistic loses faster against opponents who
//! want to win; paranoid is the one to use against another copy of this engine.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use shakmaty::{Bitboard, Chess, Color, Move, Position, Role};

use crate::eval::{self, Weights};
use crate::game::{Game, hash_of, repeats};

/// Deepest ply the search will visit.
pub const MAX_PLY: usize = 64;
/// Score of an immediate checkmate against the victim.
pub const MATE: i32 = 30_000;
/// Scores at least this large are mates rather than evaluations.
pub const MATE_IN_MAX_PLY: i32 = MATE - MAX_PLY as i32 * 2;
/// Larger than any real score.
pub const INFINITY: i32 = 32_000;

/// Upper bound on the moves shakmaty will generate in one position.
const MAX_MOVES: usize = 288;

/// How far the quiescence search may run past the nominal depth.
///
/// Under [`OpponentModel::Optimistic`] there are no beta cutoffs to contain it,
/// so this is the only thing standing between the engine and a capture-rich
/// position that takes all afternoon.
const QUIESCENCE_PLIES: u32 = 5;

/// Remaining depth at or below which the engine looks only at its most
/// promising moves rather than all of them. Full width near the root, where it
/// matters; narrow near the leaves, where it does not.
const NARROW_BELOW_DEPTH: i32 = 4;

/// How much a single exchange is assumed to be able to swing the score, beyond
/// the material actually on the two squares. Anything that cannot plausibly
/// beat what has already been found is skipped.
const DELTA_MARGIN: i32 = 200;

/// What the engine assumes the opponent is trying to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub enum OpponentModel {
    /// The opponent is playing to win, and will take what they are offered.
    #[default]
    Optimistic,
    /// The opponent is trying just as hard to lose, and will decline.
    Paranoid,
}

impl OpponentModel {
    /// The name used in UCI options and on the command line.
    pub const fn name(self) -> &'static str {
        match self {
            OpponentModel::Optimistic => "Optimistic",
            OpponentModel::Paranoid => "Paranoid",
        }
    }
}

impl fmt::Display for OpponentModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for OpponentModel {
    type Err = String;

    fn from_str(text: &str) -> Result<OpponentModel, String> {
        match text.to_ascii_lowercase().as_str() {
            "optimistic" | "cooperative" | "greedy" => Ok(OpponentModel::Optimistic),
            "paranoid" | "adversarial" | "hostile" => Ok(OpponentModel::Paranoid),
            other => Err(format!("unknown opponent model {other:?}")),
        }
    }
}

/// Everything about the engine that a user can turn a knob on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Transposition table size in mebibytes.
    pub hash_mb: usize,
    /// What the opponent is assumed to want.
    pub model: OpponentModel,
    /// How many opponent replies to consider at each of their turns, under
    /// [`OpponentModel::Optimistic`]. Their most punishing moves come first, so
    /// a small number here still catches the ways they can hurt us.
    pub opponent_moves: usize,
    /// How many of its own moves to weigh once the search is close to the leaves.
    ///
    /// Near the root every move is considered. Deeper down the move ordering is
    /// trustworthy enough to look only at the most promising ways of throwing
    /// the game, which is what makes the wide optimistic search affordable.
    pub own_moves: usize,
    /// How much worse than an equal position a draw is considered to be.
    /// Raising this makes the engine work harder to keep a lost game alive.
    pub draw_penalty: i32,
    /// How much credit to give a chance the opponent could decline, in
    /// centipawns.
    ///
    /// A mate the opponent has to *choose* to play is encouraging, not
    /// decisive. Set this very high and the engine banks on being punished,
    /// stops working to worsen its position, and drifts. Set it to zero and it
    /// only plays for defeats that hold up however the opponent replies.
    pub opportunity_bonus: i32,
    /// How badly the engine wants to lose, from 0 to 100. At 100 it plays for
    /// its own destruction; at 0 it plays ordinary chess and tries to win; at
    /// 50 it does not care either way.
    pub malice: i32,
    /// Evaluation weights.
    pub weights: Weights,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            hash_mb: 16,
            model: OpponentModel::default(),
            opponent_moves: 4,
            own_moves: 10,
            draw_penalty: 60,
            opportunity_bonus: 500,
            malice: 100,
            weights: Weights::default(),
        }
    }
}

/// When the search should stop.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    /// Stop after this many plies of iterative deepening.
    pub depth: Option<u32>,
    /// Stop after roughly this many nodes.
    pub nodes: Option<u64>,
    /// Spend exactly this long.
    pub movetime: Option<Duration>,
    /// Time left on white's clock.
    pub white_time: Option<Duration>,
    /// Time left on black's clock.
    pub black_time: Option<Duration>,
    /// White's increment per move.
    pub white_increment: Option<Duration>,
    /// Black's increment per move.
    pub black_increment: Option<Duration>,
    /// Moves left until the next time control.
    pub moves_to_go: Option<u32>,
    /// Search until told to stop.
    pub infinite: bool,
    /// If non-empty, consider only these moves at the root. This is UCI's
    /// `searchmoves`, which a client uses to ask about a specific candidate.
    pub search_moves: Vec<Move>,
    /// Slack subtracted from the clock to cover the trip through the GUI.
    pub move_overhead: Duration,
}

impl Limits {
    /// Search to a fixed depth.
    pub fn depth(depth: u32) -> Limits {
        Limits {
            depth: Some(depth),
            ..Limits::default()
        }
    }

    /// Search for a fixed wall-clock duration.
    pub fn movetime(duration: Duration) -> Limits {
        Limits {
            movetime: Some(duration),
            ..Limits::default()
        }
    }

    /// Search a fixed number of nodes.
    pub fn nodes(nodes: u64) -> Limits {
        Limits {
            nodes: Some(nodes),
            ..Limits::default()
        }
    }

    /// Whether a depth is the only thing bounding this search, so nothing caps
    /// how long reaching it may take.
    pub fn is_depth_only(&self) -> bool {
        !self.infinite
            && self.depth.is_some()
            && self.nodes.is_none()
            && self.movetime.is_none()
            && self.white_time.is_none()
            && self.black_time.is_none()
    }

    /// Whether nothing here bounds the search, in which case it should run
    /// until it is told to stop.
    pub fn is_unbounded(&self) -> bool {
        !self.infinite
            && self.depth.is_none()
            && self.nodes.is_none()
            && self.movetime.is_none()
            && self.white_time.is_none()
            && self.black_time.is_none()
    }

    /// How long the search may run, given whose clock is ticking.
    fn budget(&self, turn: Color) -> Option<Duration> {
        if self.infinite {
            return None;
        }
        if let Some(movetime) = self.movetime {
            return Some(subtract_overhead(movetime, self.move_overhead));
        }
        let (remaining, increment) = match turn {
            Color::White => (self.white_time?, self.white_increment),
            Color::Black => (self.black_time?, self.black_increment),
        };
        let increment = increment.unwrap_or_default();
        let moves = self.moves_to_go.unwrap_or(30).max(1);
        let share = remaining / moves + increment * 3 / 4;
        let ceiling = remaining * 2 / 5;
        Some(subtract_overhead(share.min(ceiling), self.move_overhead))
    }
}

fn subtract_overhead(budget: Duration, overhead: Duration) -> Duration {
    budget
        .saturating_sub(overhead)
        .max(Duration::from_millis(1))
}

/// A score as a GUI expects to see it: positive means the engine is winning,
/// which for this engine is bad news.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Score {
    /// An evaluation in centipawns.
    Centipawns(i32),
    /// Mate in this many moves. Negative means the engine is the one getting
    /// mated, which is the happy ending.
    Mate(i32),
}

impl fmt::Display for Score {
    /// Formats the way UCI wants it, as the tail of an `info score ...` field.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Score::Centipawns(value) => write!(f, "cp {value}"),
            Score::Mate(moves) => write!(f, "mate {moves}"),
        }
    }
}

/// A progress report from one completed iteration.
#[derive(Clone, Debug)]
pub struct SearchInfo<'a> {
    /// Iterative deepening depth just finished.
    pub depth: u32,
    /// Deepest ply reached anywhere in that iteration.
    pub seldepth: u32,
    /// The score, in GUI orientation.
    pub score: Score,
    /// The same position in the engine's own terms: how badly it is doing, with
    /// higher being better as far as it is concerned.
    pub sabotage: i32,
    /// Nodes visited so far this search.
    pub nodes: u64,
    /// Time spent so far.
    pub elapsed: Duration,
    /// The line the engine expects, starting with the move it intends to play.
    pub pv: &'a [Move],
}

impl SearchInfo<'_> {
    /// Nodes per second, rounded down.
    pub fn nps(&self) -> u64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds <= 0.0 {
            return 0;
        }
        (self.nodes as f64 / seconds) as u64
    }
}

/// What a finished search decided.
#[derive(Clone, Debug)]
pub struct SearchResult {
    /// The move to play, or `None` if the position has no legal moves.
    pub best_move: Option<Move>,
    /// The score of that move, in GUI orientation.
    pub score: Score,
    /// How badly the engine expects to be doing, in its own terms.
    pub sabotage: i32,
    /// Depth of the last completed iteration.
    pub depth: u32,
    /// Total nodes visited.
    pub nodes: u64,
    /// Wall-clock time spent.
    pub elapsed: Duration,
    /// The principal variation from the last completed iteration.
    pub pv: Vec<Move>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bound {
    Exact,
    Lower,
    Upper,
}

#[derive(Clone, Copy)]
struct Entry {
    key: u64,
    best: Option<Move>,
    score: i32,
    depth: i16,
    bound: Bound,
}

impl Default for Entry {
    fn default() -> Entry {
        Entry {
            key: 0,
            best: None,
            score: 0,
            depth: -1,
            bound: Bound::Exact,
        }
    }
}

/// A fixed-size, always-replace-on-shallower transposition table.
struct Table {
    entries: Vec<Entry>,
    mask: usize,
}

impl Table {
    fn with_capacity_mb(megabytes: usize) -> Table {
        let target = (megabytes * 1024 * 1024 / size_of::<Entry>()).max(1);
        let count = if target.is_power_of_two() {
            target
        } else {
            (target.next_power_of_two() / 2).max(1)
        };
        Table {
            entries: vec![Entry::default(); count],
            mask: count - 1,
        }
    }

    fn clear(&mut self) {
        self.entries.fill(Entry::default());
    }

    fn probe(&self, key: u64) -> Option<Entry> {
        let entry = self.entries[key as usize & self.mask];
        (entry.key == key && entry.depth >= 0).then_some(entry)
    }

    fn store(&mut self, key: u64, best: Option<Move>, score: i32, depth: i16, bound: Bound) {
        let slot = &mut self.entries[key as usize & self.mask];
        // Keep the deeper result unless it is about a different position.
        if slot.key == key && slot.depth > depth && slot.bound == Bound::Exact {
            return;
        }
        *slot = Entry {
            key,
            best,
            score,
            depth,
            bound,
        };
    }
}

/// Everything the ordering needs that is worth computing once per node rather
/// than once per move.
struct Context {
    /// Squares the side *not* to move attacks, i.e. where it is dangerous to
    /// put a piece down.
    hostile: Bitboard,
}

impl Context {
    fn new(position: &Chess) -> Context {
        let board = position.board();
        Context {
            hostile: eval::attack_map(board, position.turn().other(), board.occupied()),
        }
    }
}

/// The engine: options, a transposition table, and the state of one search.
pub struct Engine {
    /// Tunable settings. Changing [`Options::hash_mb`] takes effect on the next
    /// call to [`Engine::resize_table`].
    pub options: Options,
    table: Table,

    victim: Color,
    /// `100` for full sabotage, `-100` for ordinary chess, scaled by malice.
    factor: i32,
    stop: Arc<AtomicBool>,
    deadline: Option<Instant>,
    node_limit: Option<u64>,
    can_abort: bool,
    aborted: bool,
    nodes: u64,
    seldepth: usize,
    path: Vec<u64>,
    killers: Vec<[Option<Move>; 2]>,
    pv: Vec<Vec<Move>>,
}

impl Default for Engine {
    fn default() -> Engine {
        Engine::new()
    }
}

impl Engine {
    /// An engine with default options.
    pub fn new() -> Engine {
        Engine::with_options(Options::default())
    }

    /// An engine with the given options.
    pub fn with_options(options: Options) -> Engine {
        let table = Table::with_capacity_mb(options.hash_mb);
        Engine {
            options,
            table,
            victim: Color::White,
            factor: 100,
            stop: Arc::new(AtomicBool::new(false)),
            deadline: None,
            node_limit: None,
            can_abort: false,
            aborted: false,
            nodes: 0,
            seldepth: 0,
            path: Vec::with_capacity(MAX_PLY * 2),
            killers: vec![[None; 2]; MAX_PLY + 1],
            pv: (0..=MAX_PLY).map(|_| Vec::with_capacity(MAX_PLY)).collect(),
        }
    }

    /// Rebuild the transposition table at the size in [`Options::hash_mb`].
    pub fn resize_table(&mut self) {
        self.table = Table::with_capacity_mb(self.options.hash_mb);
    }

    /// Forget everything learned about previous positions.
    pub fn clear(&mut self) {
        self.table.clear();
        for killers in &mut self.killers {
            *killers = [None; 2];
        }
    }

    /// Search a position and return the move to play, ignoring progress
    /// reports. Convenient for tests and for embedding.
    pub fn best_move(&mut self, game: &Game, limits: &Limits) -> Option<Move> {
        let stop = Arc::new(AtomicBool::new(false));
        self.search(game, limits, stop, &mut |_| {}).best_move
    }

    /// Search a position, reporting each completed iteration to `on_info`.
    ///
    /// Setting `stop` asks the search to return as soon as it can; it will
    /// always return a legal move if one exists.
    pub fn search(
        &mut self,
        game: &Game,
        limits: &Limits,
        stop: Arc<AtomicBool>,
        on_info: &mut dyn FnMut(&SearchInfo<'_>),
    ) -> SearchResult {
        let started = Instant::now();
        let position = game.position().clone();

        self.victim = position.turn();
        self.factor = (self.options.malice.clamp(0, 100) * 2) - 100;
        self.stop = stop;
        self.node_limit = limits.nodes;
        self.deadline = limits.budget(self.victim).map(|budget| started + budget);
        self.can_abort = false;
        self.aborted = false;
        self.nodes = 0;
        self.seldepth = 0;
        self.path.clear();
        self.path.extend_from_slice(game.keys());
        for killers in &mut self.killers {
            *killers = [None; 2];
        }

        let legal = position.legal_moves();
        let mut roots: Vec<RootMove> = legal
            .iter()
            .filter(|m| limits.search_moves.is_empty() || limits.search_moves.contains(m))
            .map(|&m| RootMove {
                m,
                score: -INFINITY,
            })
            .collect();

        // Start in sabotage order rather than the order the moves happened to
        // be generated in. Where two moves come back with the same score the
        // first one searched wins, and that tie should go to the move that
        // looks worse for us, not to whichever the move generator emitted first.
        let context = Context::new(&position);
        roots.sort_by_key(|root| -self.order_key(&position, root.m, &context, 0, None, true));

        if roots.is_empty() {
            return SearchResult {
                best_move: None,
                score: Score::Centipawns(0),
                sabotage: 0,
                depth: 0,
                nodes: 0,
                elapsed: started.elapsed(),
                pv: Vec::new(),
            };
        }

        let mut best = roots[0].m;
        let mut best_sabotage = 0;
        let mut best_depth = 0;
        let mut best_pv = vec![best];

        let max_depth = limits.depth.unwrap_or(MAX_PLY as u32 - 4).max(1);
        for depth in 1..=max_depth {
            let mut alpha = -INFINITY;
            let mut iteration_best = None;
            let mut iteration_pv = Vec::new();

            for (index, root) in roots.iter_mut().enumerate() {
                let mut child = position.clone();
                child.play_unchecked(root.m);
                self.path.push(hash_of(&child));
                let score = self.visit(&child, depth as i32 - 1, 1, alpha, INFINITY);
                self.path.pop();

                if self.aborted {
                    break;
                }
                self.can_abort = true;

                root.score = score;
                if index == 0 || score > alpha {
                    alpha = score;
                    iteration_best = Some(root.m);
                    iteration_pv = std::iter::once(root.m)
                        .chain(self.pv[1].iter().copied())
                        .collect();
                }
            }

            if self.aborted {
                break;
            }

            // Best first, so the next iteration prunes and beams well.
            roots.sort_by(|a, b| b.score.cmp(&a.score));

            if let Some(m) = iteration_best {
                best = m;
                best_sabotage = self.orient(alpha);
                best_depth = depth;
                best_pv = iteration_pv;
            }

            let info = SearchInfo {
                depth,
                seldepth: self.seldepth as u32,
                score: self.score_for_gui(alpha),
                sabotage: best_sabotage,
                nodes: self.nodes,
                elapsed: started.elapsed(),
                pv: &best_pv,
            };
            on_info(&info);

            // A forced result is as far as thinking can take us.
            if alpha.abs() >= MATE_IN_MAX_PLY || self.out_of_time() {
                break;
            }
        }

        SearchResult {
            best_move: Some(best),
            score: self.score_for_gui(self.orient(best_sabotage)),
            sabotage: best_sabotage,
            depth: best_depth,
            nodes: self.nodes,
            elapsed: started.elapsed(),
            pv: best_pv,
        }
    }

    /// The core recursion. Returns the value of `position` in internal
    /// orientation: higher is better for whatever the engine is trying to do.
    fn visit(
        &mut self,
        position: &Chess,
        depth: i32,
        ply: usize,
        mut alpha: i32,
        mut beta: i32,
    ) -> i32 {
        self.pv[ply].clear();
        if self.check_stop() {
            return 0;
        }
        self.nodes += 1;
        self.seldepth = self.seldepth.max(ply);

        if self.is_drawn(position) {
            return self.draw_score();
        }

        let key = hash_of(position);
        let moves = position.legal_moves();
        if moves.is_empty() {
            return if position.is_check() {
                self.checkmate_score(position.turn(), ply)
            } else {
                self.draw_score()
            };
        }
        if depth <= 0 || ply + 2 >= MAX_PLY {
            return self.quiescence(position, ply, QUIESCENCE_PLIES, alpha, beta);
        }

        let maximizing = self.maximizes(position.turn());
        let entry_alpha = alpha;
        let entry_beta = beta;

        let mut tt_move = None;
        if let Some(entry) = self.table.probe(key) {
            tt_move = entry.best;
            if entry.depth >= depth as i16 {
                let score = from_table(entry.score, ply);
                let usable = match entry.bound {
                    Bound::Exact => true,
                    Bound::Lower => score >= beta,
                    Bound::Upper => score <= alpha,
                };
                if usable {
                    return score;
                }
            }
        }

        let context = Context::new(position);
        let mut ordered = moves;
        let mut keys = [0i32; MAX_MOVES];
        for (index, m) in ordered.iter().enumerate() {
            keys[index] = self.order_key(position, *m, &context, ply, tt_move, maximizing);
        }

        // Under the optimistic model the opponent is playing to win, and the
        // moves that hurt most are already at the front; looking at all of them
        // buys very little and costs a great deal.
        let hopeful = maximizing && position.turn() != self.victim;
        let considered = if hopeful {
            self.options.opponent_moves.clamp(1, ordered.len())
        } else if depth <= NARROW_BELOW_DEPTH {
            self.options.own_moves.clamp(1, ordered.len())
        } else {
            ordered.len()
        };

        let mut best_score = if maximizing { -INFINITY } else { INFINITY };
        let mut best_move = None;
        // The worst the opponent could do to our plans, used at their nodes to
        // work out what this position is really worth.
        let mut floor = INFINITY;

        for index in 0..considered {
            let mut pick = index;
            for candidate in index + 1..ordered.len() {
                if keys[candidate] > keys[pick] {
                    pick = candidate;
                }
            }
            ordered.swap(index, pick);
            keys.swap(index, pick);
            let m = ordered[index];

            let mut child = position.clone();
            child.play_unchecked(m);
            self.path.push(hash_of(&child));
            let score = self.visit(&child, depth - 1, ply + 1, alpha, beta);
            self.path.pop();

            if self.aborted {
                return best_score;
            }

            floor = floor.min(score);
            let improved = if maximizing {
                score > best_score
            } else {
                score < best_score
            };
            if improved {
                best_score = score;
                best_move = Some(m);
                self.update_pv(ply, m);
            }

            if maximizing {
                alpha = alpha.max(best_score);
            } else {
                beta = beta.min(best_score);
            }
            if alpha >= beta {
                if m.capture().is_none() && !m.is_promotion() {
                    self.remember_killer(ply, m);
                }
                break;
            }
            // Nothing beats being mated right now. At the opponent's own nodes
            // this does not apply: a mate they merely *could* play settles
            // nothing, which is what the floor below is about.
            if maximizing && !hopeful && best_score >= MATE - (ply as i32 + 1) {
                break;
            }
        }

        if hopeful && !self.aborted {
            // Everything searched so far was chosen for being the most punishing
            // reply available. That says what happens if the opponent takes the
            // chance, but nothing about what happens if they do not, and an
            // engine that banks on being punished stops trying the moment any
            // punishment becomes available. So look at their most reluctant
            // reply too, and value the position from there.
            if considered < ordered.len()
                && let Some(pick) = (considered..ordered.len()).min_by_key(|&index| keys[index])
            {
                let m = ordered[pick];
                let mut child = position.clone();
                child.play_unchecked(m);
                self.path.push(hash_of(&child));
                let score = self.visit(&child, depth - 1, ply + 1, alpha, beta);
                self.path.pop();
                if self.aborted {
                    return best_score;
                }
                floor = floor.min(score);
            }
            best_score = self.discount_a_declinable_chance(best_score, floor);
        }

        let bound = if best_score <= entry_alpha {
            Bound::Upper
        } else if best_score >= entry_beta {
            Bound::Lower
        } else {
            Bound::Exact
        };
        self.table.store(
            key,
            best_move,
            to_table(best_score, ply),
            depth as i16,
            bound,
        );

        best_score
    }

    /// Play out captures and promotions so the evaluation is not applied in the
    /// middle of a trade.
    ///
    /// `remaining` bounds how far this can go. Captures run out on their own,
    /// but check evasions do not: a long series of checks would otherwise
    /// recurse past [`MAX_PLY`].
    fn quiescence(
        &mut self,
        position: &Chess,
        ply: usize,
        remaining: u32,
        mut alpha: i32,
        mut beta: i32,
    ) -> i32 {
        if self.check_stop() {
            return 0;
        }
        self.nodes += 1;
        self.seldepth = self.seldepth.max(ply);

        // Repetition is not checked here. Everything the quiescence search
        // follows is a capture or a promotion, which resets the halfmove clock
        // and makes repetition impossible, and hashing every node to prove that
        // costs more than it is worth.
        if position.halfmoves() >= 100 || position.is_insufficient_material() {
            return self.draw_score();
        }
        if remaining == 0 || ply + 2 >= MAX_PLY {
            return self.leaf_score(position);
        }

        let maximizing = self.maximizes(position.turn());
        let in_check = position.is_check();

        let mut moves;
        let mut best_score;
        let mut stand_pat = 0;
        if in_check {
            // Evasions are not captures, so they do not shrink the position the
            // way the rest of the quiescence search does. Only follow them
            // while there is real budget left, or a long series of checks would
            // grow without bound.
            moves = position.legal_moves();
            if moves.is_empty() {
                return self.checkmate_score(position.turn(), ply);
            }
            if remaining < QUIESCENCE_PLIES - 1 {
                return self.leaf_score(position);
            }
            best_score = if maximizing { -INFINITY } else { INFINITY };
        } else {
            stand_pat = self.leaf_score(position);
            if maximizing {
                if stand_pat >= beta {
                    return stand_pat;
                }
                alpha = alpha.max(stand_pat);
            } else {
                if stand_pat <= alpha {
                    return stand_pat;
                }
                beta = beta.min(stand_pat);
            }
            best_score = stand_pat;
            moves = position.capture_moves();
            for m in position.promotion_moves() {
                if !moves.contains(&m) && !moves.is_full() {
                    moves.push(m);
                }
            }
            if moves.is_empty() {
                return stand_pat;
            }
        }

        let mut keys = [0i32; MAX_MOVES];
        for (index, m) in moves.iter().enumerate() {
            keys[index] = self.exchange_key(position, *m, maximizing);
        }

        for index in 0..moves.len() {
            let mut pick = index;
            for candidate in index + 1..moves.len() {
                if keys[candidate] > keys[pick] {
                    pick = candidate;
                }
            }
            moves.swap(index, pick);
            keys.swap(index, pick);
            let m = moves[index];

            // Delta pruning: an exchange can only move the score so far, and
            // anything that cannot reach what has already been found is not
            // worth playing out.
            if !in_check && maximizing {
                let swing = eval::piece_value(m.capture().unwrap_or(Role::Pawn))
                    + eval::piece_value(m.role())
                    + DELTA_MARGIN;
                if stand_pat + swing <= best_score {
                    continue;
                }
            }

            let mut child = position.clone();
            child.play_unchecked(m);
            let score = self.quiescence(&child, ply + 1, remaining - 1, alpha, beta);

            if self.aborted {
                return best_score;
            }

            if maximizing {
                best_score = best_score.max(score);
                alpha = alpha.max(best_score);
            } else {
                best_score = best_score.min(score);
                beta = beta.min(best_score);
            }
            if alpha >= beta {
                break;
            }
        }

        best_score
    }

    /// How attractive a move is at this node, largest first.
    fn order_key(
        &self,
        position: &Chess,
        m: Move,
        context: &Context,
        ply: usize,
        tt_move: Option<Move>,
        maximizing: bool,
    ) -> i32 {
        if Some(m) == tt_move {
            return 1_000_000;
        }

        // First, how much this move costs the player making it.
        let mut cost = 0;
        if let Some(captured) = m.capture() {
            cost -= eval::piece_value(captured);
        }
        if let Some(promoted) = m.promotion() {
            cost -= eval::piece_value(promoted) - eval::piece_value(Role::Pawn);
        }

        let to = m.to();
        if context.hostile.contains(to) {
            let value = eval::piece_value(m.role());
            let board = position.board();
            let from = m
                .from()
                .map(Bitboard::from_square)
                .unwrap_or(Bitboard::EMPTY);
            let occupied = (board.occupied() ^ from) | Bitboard::from_square(to);
            let defenders = board.attacks_to(to, position.turn(), occupied) & !from;
            cost += if defenders.any() { value / 4 } else { value };
        }

        // Then, which way that cuts for the side we want to see lose.
        let mut key = if position.turn() == self.victim {
            cost
        } else {
            -cost
        };
        if !maximizing {
            key = -key;
        }
        if self.killers[ply].contains(&Some(m)) {
            key += 5_000;
        }
        key
    }

    /// Ordering for the quiescence search, where every move is a capture or a
    /// promotion and the attack maps [`order_key`](Self::order_key) needs are
    /// not worth building.
    fn exchange_key(&self, position: &Chess, m: Move, maximizing: bool) -> i32 {
        // Taking with a big piece tends to cost the taker it; taking something
        // valuable tends to pay. Both read backwards for the side losing.
        let mut cost = eval::piece_value(m.role()) / 2;
        if let Some(captured) = m.capture() {
            cost -= eval::piece_value(captured);
        }
        if let Some(promoted) = m.promotion() {
            cost -= eval::piece_value(promoted) - eval::piece_value(Role::Pawn);
        }
        let key = if position.turn() == self.victim {
            cost
        } else {
            -cost
        };
        if maximizing { key } else { -key }
    }

    fn remember_killer(&mut self, ply: usize, m: Move) {
        let killers = &mut self.killers[ply];
        if killers[0] != Some(m) {
            killers[1] = killers[0];
            killers[0] = Some(m);
        }
    }

    fn update_pv(&mut self, ply: usize, m: Move) {
        let (head, tail) = self.pv.split_at_mut(ply + 1);
        let line = &mut head[ply];
        line.clear();
        line.push(m);
        line.extend_from_slice(&tail[0]);
    }

    /// What a position is worth to us when the opponent has a chance to punish
    /// us but does not have to take it.
    ///
    /// `best` is what happens if they do; `floor` is what happens if they play
    /// their most reluctant reply instead. Banking on `best` is what makes an
    /// engine stop trying: any position where mate is merely *available* scores
    /// the same as any other, so every move looks equally good and it shuffles.
    /// So the value is the floor, plus a bounded amount of credit for the
    /// chance. Only a defeat the opponent cannot decline counts as a real mate.
    fn discount_a_declinable_chance(&self, best: i32, floor: i32) -> i32 {
        if best <= floor {
            return floor;
        }
        if floor >= MATE_IN_MAX_PLY {
            // Every reply mates us. That one is in the bag.
            return floor;
        }
        let credit = (best - floor).min(self.options.opportunity_bonus.max(0));
        (floor + credit).min(MATE_IN_MAX_PLY - 1)
    }

    /// Whether the player to move is trying to make the score go up.
    fn maximizes(&self, turn: Color) -> bool {
        turn == self.victim || self.options.model == OpponentModel::Optimistic
    }

    /// Whether the game is already over as a draw. The current position's hash
    /// is the last entry on the path, so repetition needs no extra bookkeeping.
    fn is_drawn(&self, position: &Chess) -> bool {
        position.halfmoves() >= 100
            || position.is_insufficient_material()
            || repeats(&self.path, position.halfmoves())
    }

    /// Flip a badness score into internal orientation, and back again. At full
    /// malice this does nothing; below 50 the engine wants the opposite of
    /// everything, mates and draws included.
    fn orient(&self, score: i32) -> i32 {
        if self.factor >= 0 { score } else { -score }
    }

    fn leaf_score(&self, position: &Chess) -> i32 {
        eval::evaluate_with(position, self.victim, &self.options.weights) * self.factor / 100
    }

    fn checkmate_score(&self, mated: Color, ply: usize) -> i32 {
        let distance = MATE - ply as i32;
        self.orient(if mated == self.victim {
            distance
        } else {
            -distance
        })
    }

    fn draw_score(&self) -> i32 {
        self.orient(-self.options.draw_penalty)
    }

    /// Convert an internal score into what a GUI expects: positive when the
    /// engine is winning, negative when it is losing, mate distances in moves.
    fn score_for_gui(&self, internal: i32) -> Score {
        let badness = self.orient(internal);
        if badness >= MATE_IN_MAX_PLY {
            Score::Mate(-((MATE - badness + 1) / 2))
        } else if badness <= -MATE_IN_MAX_PLY {
            Score::Mate((MATE + badness + 1) / 2)
        } else {
            Score::Centipawns(-badness)
        }
    }

    fn out_of_time(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn check_stop(&mut self) -> bool {
        if self.aborted {
            return true;
        }
        if !self.can_abort {
            return false;
        }
        if self.node_limit.is_some_and(|limit| self.nodes >= limit) {
            self.aborted = true;
            return true;
        }
        if self.nodes.is_multiple_of(2048)
            && (self.stop.load(Ordering::Relaxed) || self.out_of_time())
        {
            self.aborted = true;
            return true;
        }
        false
    }
}

struct RootMove {
    m: Move,
    score: i32,
}

/// Mate scores are stored relative to the root, not to the node that found
/// them, so an entry stays true wherever it is probed from.
fn to_table(score: i32, ply: usize) -> i32 {
    if score >= MATE_IN_MAX_PLY {
        score + ply as i32
    } else if score <= -MATE_IN_MAX_PLY {
        score - ply as i32
    } else {
        score
    }
}

fn from_table(score: i32, ply: usize) -> i32 {
    if score >= MATE_IN_MAX_PLY {
        score - ply as i32
    } else if score <= -MATE_IN_MAX_PLY {
        score + ply as i32
    } else {
        score
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        let mut engine = Engine::new();
        engine.options.hash_mb = 1;
        engine.resize_table();
        engine
    }

    fn game(fen: &str) -> Game {
        Game::from_fen(fen).unwrap_or_else(|e| panic!("{fen}: {e}"))
    }

    fn best(engine: &mut Engine, game: &Game, depth: u32) -> String {
        let m = engine
            .best_move(game, &Limits::depth(depth))
            .expect("a legal move");
        shakmaty::uci::UciMove::from_standard(m).to_string()
    }

    #[test]
    fn it_walks_into_mate_in_one() {
        // Black threatens Ra1#, but only while white's king stays boxed in by
        // its own pawns. Kh1 keeps the box shut; every pawn move opens an
        // escape square and every other king move walks out of the net.
        let position = game("r5k1/8/8/8/8/8/5PPP/6K1 w - - 0 1");
        let mut engine = engine();
        assert_eq!(
            best(&mut engine, &position, 3),
            "g1h1",
            "the only move that preserves the mate must be the chosen one"
        );
    }

    #[test]
    fn it_refuses_to_deliver_mate() {
        // Qe8 is mate on the back rank. Anything else keeps the game, and the
        // chance of losing it, alive.
        let position = game("6k1/5ppp/8/8/8/8/5PPP/4Q1K1 w - - 0 1");
        let mut engine = engine();
        let chosen = best(&mut engine, &position, 4);
        assert_ne!(chosen, "e1e8", "mating the opponent is the worst outcome");
    }

    #[test]
    fn it_declines_a_free_queen() {
        // Rxd8 wins the black queen for nothing. Taking it would be doubly
        // wrong: material the engine does not want, and the removal of the one
        // piece black has to mate it with.
        let position = game("3q2k1/8/8/8/8/6P1/7P/3R2K1 w - - 0 1");
        let mut engine = engine();
        assert_ne!(
            best(&mut engine, &position, 4),
            "d1d8",
            "a free queen is a trap"
        );
    }

    #[test]
    fn it_will_not_capture_its_way_into_a_dead_draw() {
        // Black's king can take the undefended queen, leaving king against
        // king. That is a draw, and a draw is half a point too many; better to
        // leave the queen alive and be mated by it.
        let position = game("8/8/8/6k1/7Q/8/8/4K3 b - - 0 1");
        let mut engine = engine();
        assert_ne!(
            best(&mut engine, &position, 4),
            "g5h4",
            "taking the last mating piece off the board ends the game as a draw"
        );
    }

    #[test]
    fn the_value_system_ranks_defeat_above_a_draw_above_victory() {
        let engine = engine();
        let victim = engine.victim;
        let mated = engine.checkmate_score(victim, 4);
        let mating = engine.checkmate_score(victim.other(), 4);
        let draw = engine.draw_score();
        let losing_badly = 500;
        let winning_badly = -500;

        assert!(
            mated > losing_badly,
            "being mated is the best result of all"
        );
        assert!(losing_badly > draw, "a game still being lost beats a draw");
        assert!(draw > winning_badly, "a draw beats being ahead");
        assert!(
            winning_badly > mating,
            "delivering mate is the worst result"
        );
    }

    #[test]
    fn with_no_malice_it_plays_to_win() {
        // Same mate in one, but now the engine wants it.
        let position = game("6k1/5ppp/8/8/8/8/5PPP/4Q1K1 w - - 0 1");
        let mut engine = engine();
        engine.options.malice = 0;
        engine.options.model = OpponentModel::Paranoid;
        assert_eq!(
            best(&mut engine, &position, 3),
            "e1e8",
            "at zero malice it should be an ordinary, if weak, chess engine"
        );
    }

    #[test]
    fn both_opponent_models_produce_legal_moves() {
        let position = game("r1bqkbnr/pppp1ppp/2n5/4p3/2B1P3/5Q2/PPPP1PPP/RNB1K1NR w KQkq - 4 4");
        for model in [OpponentModel::Optimistic, OpponentModel::Paranoid] {
            let mut engine = engine();
            engine.options.model = model;
            let m = engine
                .best_move(&position, &Limits::depth(4))
                .expect("a legal move");
            assert!(
                position.position().is_legal(m),
                "{model} produced the illegal move {m}"
            );
        }
    }

    #[test]
    fn a_position_with_no_moves_returns_no_move() {
        let stalemate = game("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1");
        let mut engine = engine();
        let result = engine.search(
            &stalemate,
            &Limits::depth(3),
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        );
        assert!(result.best_move.is_none());
    }

    #[test]
    fn a_chance_the_opponent_can_decline_is_not_worth_a_mate() {
        let engine = engine();
        let bonus = engine.options.opportunity_bonus;

        // Mate is available but only if the opponent chooses it. The position
        // is worth what happens when they do not, plus a bounded amount of
        // credit for the chance. Crucially it is no longer a mate score, so
        // every other move does not tie with it.
        let hopeful = engine.discount_a_declinable_chance(MATE - 3, 100);
        assert_eq!(hopeful, 100 + bonus);
        assert!(hopeful < MATE_IN_MAX_PLY, "a declinable mate is not a mate");

        // When every reply mates us, it really is mate.
        let forced = engine.discount_a_declinable_chance(MATE - 3, MATE - 5);
        assert_eq!(forced, MATE - 5);
        assert!(forced >= MATE_IN_MAX_PLY);

        // Ordinary upside is credited in full up to the cap, so positions with
        // worse floors still rank above positions with better ones.
        assert_eq!(engine.discount_a_declinable_chance(300, 100), 300);
        assert_eq!(engine.discount_a_declinable_chance(9000, 100), 100 + bonus);
        assert!(
            engine.discount_a_declinable_chance(9000, 100)
                < engine.discount_a_declinable_chance(9000, 400),
            "a worse floor must still win, or the engine stops trying to reach one"
        );

        // Nothing to hope for.
        assert_eq!(engine.discount_a_declinable_chance(-50, -50), -50);
    }

    #[test]
    fn it_does_not_settle_for_a_mate_the_opponent_can_refuse() {
        // After 1.e4 f5 2.Nf3 g5 white *could* play Qh5#. Banking on that used
        // to make every move score the same, and the engine drifted. The score
        // here has to be an evaluation, not a mate.
        let position = game("rnbqkbnr/pppp1p1p/8/5pp1/4P3/5N2/PPPP1PPP/RNBQKB1R b KQkq - 0 3");
        let mut engine = engine();
        let result = engine.search(
            &position,
            &Limits::depth(4),
            Arc::new(AtomicBool::new(false)),
            &mut |_| {},
        );
        assert!(
            !matches!(result.score, Score::Mate(_)),
            "a mate white can simply decline was reported as {:?}",
            result.score
        );
        assert!(
            result.sabotage > 500,
            "it should still be busy losing, not idling: {}",
            result.sabotage
        );
    }

    #[test]
    fn scores_are_reported_the_way_a_gui_expects() {
        let mut engine = engine();
        engine.factor = 100;
        // Badness of +900 means the engine is a queen down.
        assert_eq!(engine.score_for_gui(900), Score::Centipawns(-900));
        // Getting mated in two plies is mate in one, against us.
        assert_eq!(engine.score_for_gui(MATE - 2), Score::Mate(-1));
        // Delivering mate in one ply is a disaster reported as a win.
        assert_eq!(engine.score_for_gui(-(MATE - 1)), Score::Mate(1));
        assert_eq!(Score::Mate(-3).to_string(), "mate -3");
        assert_eq!(Score::Centipawns(-42).to_string(), "cp -42");
    }

    #[test]
    fn a_stop_flag_ends_the_search_promptly() {
        let position = game("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1");
        let mut engine = engine();
        let stop = Arc::new(AtomicBool::new(true));
        let result = engine.search(&position, &Limits::default(), stop, &mut |_| {});
        assert!(
            result.best_move.is_some(),
            "even an immediate stop must produce a move"
        );
    }

    #[test]
    fn time_is_divided_up_sensibly() {
        let limits = Limits {
            white_time: Some(Duration::from_secs(60)),
            white_increment: Some(Duration::from_secs(1)),
            move_overhead: Duration::from_millis(30),
            ..Limits::default()
        };
        let budget = limits.budget(Color::White).expect("a budget");
        assert!(
            budget >= Duration::from_millis(2_000) && budget <= Duration::from_millis(3_500),
            "unexpected budget {budget:?}"
        );

        let fixed = Limits::movetime(Duration::from_millis(500));
        assert_eq!(fixed.budget(Color::Black), Some(Duration::from_millis(500)));

        let endless = Limits {
            infinite: true,
            white_time: Some(Duration::from_secs(1)),
            ..Limits::default()
        };
        assert_eq!(endless.budget(Color::White), None);
    }
}
