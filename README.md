# project-zero-elo

A chess engine that actively attempts to lose every game. Its Elo should converge towards 0.

`zero-elo` speaks the Universal Chess Interface, so it drops into any chess GUI
like a normal engine. It then plays real chess in reverse: it searches for the
move that leads to the worst long-term outcome for itself, and the outcome it
wants most is being checkmated.

```
$ zero-elo selfplay
saboteur plays white, opponent is an ordinary engine

1. e4 b6 2. Ba6 Nxa6 3. Qh5 Nf6 4. b4 Nxh5 5. Bb2 Nb8 6. Nc3 d5 7. h3
dxe4 8. g3 e3 9. g4 Qxd2 10. Kf1 Qxf2

8 r n b . k b . r
7 p . p . p p p p
6 . p . . . . . .
5 . . . . . . . n
4 . P . . . . P .
3 . . N . p . . P
2 P B P . . q . .
1 R . . . . K N R
  a b c d e f g h

0-1: black wins
```

## Losing is harder than it looks

The obvious approach — flip the sign on a normal evaluation and give everything
away — does not work. An engine that only minimises its own material trades
itself down to a bare king and *draws*, and a draw is half a point the engine
did not want. Playing to lose well means holding several ideas at once:

- **Lose material, but not the opponent's.** Their queen is the instrument of
  our defeat. Capturing it is a mistake, and so is trading into an endgame where
  they no longer have the material to mate with.
- **Draws are failures, ranked below any position that is merely lost.** The
  engine's value scale runs: getting mated ≫ losing badly ≫ a draw ≫ winning ≫
  delivering mate. Stalemate, threefold repetition, the fifty-move rule and dead
  material are all treated as defeats of a worse kind than defeat.
- **Leave gifts where they will be taken.** A piece that is hanging is worth
  part of its value, because a greedy opponent will collect it.
- **Never bank on being punished.** This one is the difference between an engine
  that throws games and an engine that merely plays badly. If a position where
  the opponent *could* mate counts as a mate, then every move that keeps that
  chance alive scores identically, the engine has nothing left to choose
  between them, and it drifts — shuffling pieces while its position stays
  exactly as bad as it already was. So a chance the opponent can decline is
  worth the position it leaves behind when they decline it, plus a bounded
  amount of credit (`Opportunity Bonus`) for the chance itself. Only a defeat
  the opponent cannot avoid counts as a real mate. The engine therefore has to
  keep finding ways to make its position genuinely worse, every move.
- **Walk the king into the open.** And once nothing is left to defend with, walk
  it to the *edge* of the board, because that is where mate happens.

## Installing

```sh
cargo install zero-elo-cli
# the binary is named zero-elo
```

Or from a checkout:

```sh
cargo build --release
# the binary lands in target/release/zero-elo
```

Point your GUI at that binary and it will behave like any other UCI engine —
until you look at the evaluation.

### With chess-tui

[`chess-tui`](https://crates.io/crates/chess-tui) takes the engine path as an
argument and remembers it in its config file, so this is a one-off:

```sh
cargo install chess-tui
chess-tui -e /full/path/to/target/release/zero-elo
```

Then pick "Play against the bot" from the menu. Use an absolute path; a relative
one will not survive into the config file.

chess-tui cannot send engine-specific options, but it does split the engine path
on whitespace and pass the rest as arguments, so the settings below can be baked
in at that point instead:

```sh
chess-tui -e "/full/path/to/zero-elo uci --malice 60 --model paranoid"
```

Three things worth knowing:

- **Difficulty "Off" asks for `go depth 10` with no clock.** Nothing then bounds
  how long reaching that depth may take, and this engine cannot promise it
  quickly — the optimistic model has no alpha-beta cutoffs to prune with, and
  the paranoid one can spend minutes on a deep middlegame. The `Depth Timeout`
  option exists for exactly this: by default the engine answers within five
  seconds with the best line it found and reports the depth it actually
  reached. Set it to 0 if you would rather wait for the depth you asked for.
- **The difficulty presets send `UCI_LimitStrength` and `UCI_Elo`**, which this
  engine does not implement and ignores. Use `--malice` to make it play less
  badly.
- **It starts a fresh engine process per move and sends a plain FEN with no move
  history**, so in that setup the engine cannot see repetitions coming and may
  walk into one. A draw is a failure by its standards, so that costs it.

### UCI support

Everything a GUI needs is implemented:

| | |
| --- | --- |
| Commands in | `uci`, `debug`, `isready`, `setoption`, `ucinewgame`, `position` (`startpos` or `fen`, with `moves`), `go`, `stop`, `ponderhit`, `quit` |
| `go` arguments | `depth`, `nodes`, `movetime`, `wtime`, `btime`, `winc`, `binc`, `movestogo`, `infinite`, `searchmoves` |
| Commands out | `id`, `option`, `uciok`, `readyok`, `info` (`depth`, `seldepth`, `score cp`/`mate`, `nodes`, `nps`, `time`, `pv`), `bestmove` |

Unrecognised tokens are skipped and the rest of the line parsed, as the protocol
asks, so `joho debug on` means `debug on`.

Not implemented: pondering (no `Ponder` option is advertised, so a GUI will not
ask for it, and `ponderhit` is accepted and ignored), `go mate`, MultiPV, and
the optional `info` fields `hashfull`, `currmove` and `tbhits`. Registration and
copy protection do not apply.

There are also three non-standard commands for driving it by hand: `d` prints
the board, `eval` prints the static score, and `perft <depth>` counts nodes.

## Using it from the command line

Running `zero-elo` with no arguments starts the UCI loop on stdin and stdout.
The other subcommands are for watching it work:

| Command | What it does |
| --- | --- |
| `zero-elo analyse --fen '<fen>' --depth 8` | Search a position and print each iteration |
| `zero-elo play --side black --movetime 500` | Play a game against it in the terminal |
| `zero-elo selfplay --rival normal` | Watch it lose a full game to an ordinary engine |
| `zero-elo perft 5` | Count leaf nodes, to check move generation |
| `zero-elo bench` | Search a fixed set of positions and report the speed |

```
$ zero-elo analyse
depth  1  score     cp -25  nodes        40  f3
depth  2  score     cp -25  nodes       274  f3 a6
depth  3  score    cp -836  nodes      3010  e3 d6 Qg4
depth  4  score    cp -836  nodes     16986  e3 d6 Qg4 Bxg4
depth  5  score   cp -1386  nodes    192894  e4 d6 e5 dxe5 Ba6

best e4  (sabotage 1386, 192894 nodes in 0.05s)
```

Played out against an opponent who never takes what is offered, the engine's
opinion of itself gets steadily worse rather than levelling off — which is what
"actively trying to lose" looks like from the outside:

```
$ printf 'e4\nNf3\nBc4\nd4\nO-O\nNc3\nRe1\nQe2\n' | zero-elo play --side white
engine plays f5  (it rates itself at mate -3)
engine plays e5  (it rates itself at cp -1940)
engine plays Qh4 (it rates itself at cp -2419)
engine plays Ba3 (it rates itself at cp -2650)
engine plays f4  (it rates itself at cp -2783)
engine plays h5  (it rates itself at cp -2837)
engine plays Nf6 (it rates itself at cp -2988)
engine plays Nd5 (it rates itself at cp -3049)
```

Scores are reported the way UCI defines them, from the engine's point of view,
so a `mate -4` would mean *the engine* expects to be mated in four and is
delighted about it. The `sabotage` number is the same position in the engine's own terms,
where higher is better as far as it is concerned.

## What the opponent is assumed to want

Ordinary chess is zero-sum: what helps me hurts you, so minimax applies. Here it
does not. The engine wants to lose, and whether the opponent shares that goal is
a modelling choice rather than a fact. Both choices are available:

**Optimistic** (the default) assumes the opponent is playing to win, and
therefore wants the same thing the engine does. Both sides maximise the same
score. This is what a real opponent actually does, and it finds the quickest
defeats — but with a maximum at every node, alpha-beta has nothing to prune
against, so the search is wide and shallow. The opponent's replies are narrowed
to their most punishing few to keep it affordable.

**Paranoid** assumes the opponent is trying just as hard *not* to win: they
decline every gift and refuse every mate. That restores minimax, so alpha-beta
works and the search goes several plies deeper. What it finds are losses that
hold up no matter how uncooperative the opponent is.

Neither is strictly better. Optimistic loses faster against opponents who want
to win. Paranoid is the one to use against another copy of this engine — where,
as you would expect, two saboteurs will shuffle politely until the move limit.

## UCI options

| Option | Default | Meaning |
| --- | --- | --- |
| `Hash` | 16 | Transposition table size, in MiB |
| `Opponent Model` | `Optimistic` | `Optimistic` or `Paranoid`, as above |
| `Opponent Moves` | 4 | How many opponent replies to weigh, under `Optimistic` |
| `Own Moves` | 10 | How many of its own moves to weigh close to the leaves |
| `Opportunity Bonus` | 500 | Centipawns of credit for a chance the opponent could decline |
| `Draw Penalty` | 60 | How much worse than an equal position a draw is |
| `Malice` | 100 | How badly it wants to lose, 0–100 |
| `Move Overhead` | 30 | Milliseconds of slack for the trip through the GUI |
| `Depth Timeout` | 5000 | Milliseconds to spend on a `go depth N` with no clock; 0 for no limit |
| `Clear Hash` | — | Button |

`Malice` is a single dial between the two possible objectives. At 100 the engine
plays for its own destruction. At 0 it negates the same evaluation and plays
ordinary — if not especially strong — chess, which is what `selfplay` uses as
the opponent. At 50 it has no opinion about the result at all.

The evaluation is antisymmetric for exactly this reason: every positional term
is measured for both players and subtracted, so flipping the sign really does
produce a coherent chess player rather than a confused one. The one deliberate
exception is the family of terms about draws, which are not mirrored — a draw is
a bad outcome whichever direction you are playing in.

## Using it as a library

```rust
use zero_elo::{Engine, Game, Limits};

let mut game = Game::new();
game.play_uci("e2e4").unwrap();

let mut engine = Engine::new();
let worst = engine.best_move(&game, &Limits::depth(6)).expect("a legal move");
println!("the engine would like to play {worst}");
```

The library is split into four pieces:
[`eval`](https://docs.rs/zero-elo/latest/zero_elo/eval/) scores a position as
*badness* for whichever side is trying to lose,
[`search`](https://docs.rs/zero-elo/latest/zero_elo/search/) maximises that
score, [`game`](https://docs.rs/zero-elo/latest/zero_elo/game/) tracks the
position and the history that repetition detection needs, and
[`uci`](https://docs.rs/zero-elo/latest/zero_elo/uci/) speaks the protocol over
any reader and writer.

Move generation, FEN and the rules of chess come from
[`shakmaty`](https://crates.io/crates/shakmaty), which is re-exported so callers
do not need a matching version of their own.

## Layout

```
crates/zero-elo       the library: evaluation, search, UCI
crates/zero-elo-cli   the binary, named zero-elo
```

## Tests

```sh
cargo test --release
```

The unit tests check individual decisions: that it declines a free queen, that
it will not capture its way into a dead draw, that it refuses to deliver mate,
that a draw ranks below a defeat. The integration tests check the only thing
that really matters, which is the result — full games played out against an
ordinary engine, which the saboteur is required to lose.

## License

Apache-2.0.
