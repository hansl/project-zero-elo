//! The sabotage evaluation.
//!
//! Every score in this module is *badness*, measured from the point of view of
//! the **victim**: the player who is trying to lose. A positive score means the
//! victim's situation is satisfyingly dire. A negative score means the victim is
//! in the humiliating position of winning.
//!
//! Getting this backwards from a normal evaluation is not just a sign flip.
//! An engine that only minimises its own material trades itself down to a bare
//! king and draws, which is a *failure*: a draw scores half a point, and half a
//! point is half a point too many. So the terms below pull in several
//! directions at once:
//!
//! * lose material, but keep enough of the opponent's on the board for them to
//!   mate with;
//! * leave pieces hanging where a greedy opponent will take them;
//! * march the king into the open where it can be caught;
//! * keep enough mobility to avoid stalemate, which is also a draw.

use shakmaty::{Bitboard, Board, Chess, Color, Position, Role, Square, attacks};

/// Centipawn value of each role, used for material and for deciding how
/// generous a gift is.
pub const fn piece_value(role: Role) -> i32 {
    match role {
        Role::Pawn => 100,
        Role::Knight => 320,
        Role::Bishop => 330,
        Role::Rook => 500,
        Role::Queen => 900,
        Role::King => 0,
    }
}

/// Tunable weights for [`evaluate_with`].
///
/// All weights are in centipawns of badness. Raising one makes the engine care
/// more about that particular route to defeat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Weights {
    /// Scale applied to the material difference, as a percentage. At 100, a
    /// hanging queen is worth 900 badness.
    pub material: i32,
    /// Penalty when the opponent can no longer force mate (say, they are down
    /// to a lone bishop). Their bare king cannot punish us.
    pub no_forced_mate: i32,
    /// Penalty when the opponent cannot mate at all, even with our help. The
    /// game is heading for a dead draw.
    pub no_mate_material: i32,
    /// Penalty for the victim having so little left that stalemate looms.
    pub stalemate_risk: i32,
    /// Bonus per square around the victim's king that the opponent attacks.
    pub king_ring_attack: i32,
    /// Bonus for the victim standing in check.
    pub in_check: i32,
    /// Penalty per friendly pawn sheltering the victim's king.
    pub king_shelter: i32,
    /// Bonus per rank the victim's king has advanced from its own back rank.
    pub king_advance: i32,
    /// Bonus for the victim's king standing towards the centre files.
    pub king_centre: i32,
    /// Bonus per step a bare king stands from the centre, once the other side
    /// has only technique left to mate with. This is what drives a king to the
    /// edge of the board, where mate actually happens.
    pub mop_up_edge: i32,
    /// Bonus per step the mating king stands closer to the bare one.
    pub mop_up_kings: i32,
    /// Percentage of a piece's value earned for leaving it hanging.
    pub hanging: i32,
    /// Percentage of a piece's value earned for leaving it attacked but
    /// defended, which invites a trade rather than a gift.
    pub attacked: i32,
    /// Bonus per step the opponent's pieces stand closer to the victim's king.
    pub hunter_proximity: i32,
    /// Penalty per victim pawn on the sixth rank or beyond, since promoting
    /// would hand the victim a queen it does not want.
    pub promotion_risk: i32,
    /// Bonus per enemy pawn on the sixth rank or beyond: their new queen is
    /// another instrument of our defeat.
    pub enemy_promotion: i32,
    /// Penalty per square of victim mobility. Passive pieces cannot rescue us,
    /// but the [`Weights::stalemate_risk`] term stops this from going too far.
    pub mobility: i32,
}

impl Default for Weights {
    fn default() -> Weights {
        Weights {
            material: 100,
            no_forced_mate: 450,
            no_mate_material: 1200,
            stalemate_risk: 250,
            king_ring_attack: 14,
            in_check: 35,
            king_shelter: 25,
            king_advance: 22,
            king_centre: 14,
            mop_up_edge: 16,
            mop_up_kings: 12,
            hanging: 45,
            attacked: 12,
            hunter_proximity: 3,
            promotion_risk: 40,
            enemy_promotion: 25,
            mobility: 2,
        }
    }
}

impl Weights {
    /// Weights that score a position the way an ordinary engine would: material
    /// and nothing else. Useful as a baseline in tests.
    pub fn material_only() -> Weights {
        Weights {
            material: 100,
            ..Weights::zeroed()
        }
    }

    /// All weights set to zero.
    pub fn zeroed() -> Weights {
        Weights {
            material: 0,
            no_forced_mate: 0,
            no_mate_material: 0,
            stalemate_risk: 0,
            king_ring_attack: 0,
            in_check: 0,
            king_shelter: 0,
            king_advance: 0,
            king_centre: 0,
            mop_up_edge: 0,
            mop_up_kings: 0,
            hanging: 0,
            attacked: 0,
            hunter_proximity: 0,
            promotion_risk: 0,
            enemy_promotion: 0,
            mobility: 0,
        }
    }
}

/// Every square attacked by `color`, seen through `occupied`.
///
/// Pawns contribute their capture squares only, which is what matters for
/// deciding whether a piece is safe to leave standing somewhere.
pub fn attack_map(board: &Board, color: Color, occupied: Bitboard) -> Bitboard {
    let mut map = Bitboard::EMPTY;
    for square in board.by_color(color) {
        if let Some(piece) = board.piece_at(square) {
            map |= attacks::attacks(square, piece, occupied);
        }
    }
    map
}

/// Whether `color` could force mate against a lone king.
///
/// Two knights are excluded: that mate cannot be forced. It can still be
/// reached with cooperation, which is what [`can_ever_mate`] reports.
pub fn can_force_mate(board: &Board, color: Color) -> bool {
    let ours = board.by_color(color);
    if ((board.pawns() | board.rooks() | board.queens()) & ours).any() {
        return true;
    }
    let bishops = (board.bishops() & ours).count();
    let knights = (board.knights() & ours).count();
    bishops >= 2 || (bishops >= 1 && knights >= 1)
}

/// Whether `color` holds material that could mate at all, given a cooperative
/// opponent. This engine is nothing if not cooperative.
pub fn can_ever_mate(board: &Board, color: Color) -> bool {
    let ours = board.by_color(color);
    if ((board.pawns() | board.rooks() | board.queens()) & ours).any() {
        return true;
    }
    ((board.bishops() | board.knights()) & ours).count() >= 2
}

/// Total centipawn material for `color`, kings excluded.
pub fn material(board: &Board, color: Color) -> i32 {
    let ours = board.by_color(color);
    let mut total = 0;
    for role in [
        Role::Pawn,
        Role::Knight,
        Role::Bishop,
        Role::Rook,
        Role::Queen,
    ] {
        total += piece_value(role) * (board.by_role(role) & ours).count() as i32;
    }
    total
}

/// Score the position with the default weights.
///
/// Positive means `victim` is losing, which is the whole point.
pub fn evaluate(position: &Chess, victim: Color) -> i32 {
    evaluate_with(position, victim, &Weights::default())
}

/// Score the position with explicit weights.
///
/// The positional terms are measured for both players and subtracted, so the
/// score is antisymmetric: swapping the victim flips the sign. That matters for
/// two reasons. It stops the engine from building an attack it might blunder a
/// checkmate out of, and it means [`crate::Options::malice`] can turn the same
/// evaluation into an ordinary chess engine simply by negating it.
///
/// The exceptions are the terms about draws, which are *not* mirrored: a draw
/// is a bad outcome for a player who wants to lose and for a player who wants
/// to win, so both should steer away from one.
pub fn evaluate_with(position: &Chess, victim: Color, weights: &Weights) -> i32 {
    let board = position.board();
    let hunter = victim.other();
    let occupied = board.occupied();

    let victim_attacks = attack_map(board, victim, occupied);
    let hunter_attacks = attack_map(board, hunter, occupied);

    let victim_material = material(board, victim);
    let hunter_material = material(board, hunter);

    let mut score = (hunter_material - victim_material) * weights.material / 100;

    // Someone has to be able to finish the job. Negated, this reads as an
    // ordinary engine's wish to strip its opponent of the means to win.
    if !can_ever_mate(board, hunter) {
        score -= weights.no_mate_material;
    } else if !can_force_mate(board, hunter) {
        score -= weights.no_forced_mate;
    }

    // A player with nothing left is a player about to be stalemated.
    if (board.pawns() & board.by_color(victim)).is_empty() && victim_material <= 400 {
        score -= weights.stalemate_risk;
    }

    let in_check = position.is_check();
    score += king_danger(
        board,
        victim,
        hunter_attacks,
        in_check && position.turn() == victim,
        weights,
    );
    score -= king_danger(
        board,
        hunter,
        victim_attacks,
        in_check && position.turn() == hunter,
        weights,
    );

    score += gifts(board, victim, hunter_attacks, victim_attacks, weights);
    score -= gifts(board, hunter, victim_attacks, hunter_attacks, weights);

    // Promotion cuts both ways: our new queen would be an embarrassment,
    // theirs is another instrument of our defeat.
    score -= advanced_pawns(board, victim) * weights.promotion_risk;
    score += advanced_pawns(board, hunter) * weights.enemy_promotion;

    // Passive pieces, but the stalemate term above stops this going too far.
    let victim_mobility = (victim_attacks & !board.by_color(victim)).count() as i32;
    let hunter_mobility = (hunter_attacks & !board.by_color(hunter)).count() as i32;
    score -= (victim_mobility - hunter_mobility) * weights.mobility / 8;

    score
}

/// How much trouble `defender`'s king is in, from `attacker`'s attacks.
///
/// Positive means the king is in danger, which the victim wants and the hunter
/// does not.
fn king_danger(
    board: &Board,
    defender: Color,
    attacker_attacks: Bitboard,
    defender_in_check: bool,
    weights: &Weights,
) -> i32 {
    let Some(king) = board.king_of(defender) else {
        return 0;
    };
    let attacker = defender.other();
    if !can_ever_mate(board, attacker) {
        // No danger to measure. A king cannot be hunted down by an opponent
        // with nothing to hunt with, and pretending otherwise would stop the
        // mating king below from ever daring to walk up the board.
        return 0;
    }

    let ring = attacks::king_attacks(king) | Bitboard::from_square(king);
    let mut score = (ring & attacker_attacks).count() as i32 * weights.king_ring_attack;
    score -=
        (ring & board.pawns() & board.by_color(defender)).count() as i32 * weights.king_shelter;

    if material(board, defender) <= 100 && can_force_mate(board, attacker) {
        // Nothing left to defend with. Mate now comes from technique rather
        // than from an attack: drive this king to the edge and march the other
        // king up to help. Both engines want to know this, one so it can
        // finish the job and one so it can walk into it.
        score += centre_distance(king) * weights.mop_up_edge;
        if let Some(other) = board.king_of(attacker) {
            score += (7 - king_distance(king, other)) * weights.mop_up_kings;
        }
    } else {
        // A king that has left home, and left the safety of the flank, is a
        // king that can be hunted down.
        score += defender.relative_rank(king.rank()).to_u32() as i32 * weights.king_advance;
        let file = king.file().to_u32() as i32;
        score += (3 - (file - 3).abs().min((file - 4).abs())) * weights.king_centre;
    }

    if defender_in_check {
        score += weights.in_check;
    }

    // Mate needs the attacking pieces nearby, so reward them closing in.
    if weights.hunter_proximity != 0 {
        for square in board.by_color(defender.other()) & !board.pawns() {
            score += (7 - king_distance(square, king)) * weights.hunter_proximity;
        }
    }
    score
}

/// The value `owner` is leaving on the table for `attacker` to take.
fn gifts(
    board: &Board,
    owner: Color,
    attacker_attacks: Bitboard,
    owner_attacks: Bitboard,
    weights: &Weights,
) -> i32 {
    let mut score = 0;
    let exposed = board.by_color(owner) & attacker_attacks & !board.kings();
    for square in exposed {
        let Some(role) = board.role_at(square) else {
            continue;
        };
        // Undefended is a present; defended is merely an invitation to trade.
        let percentage = if owner_attacks.contains(square) {
            weights.attacked
        } else {
            weights.hanging
        };
        score += piece_value(role) * percentage / 100;
    }
    score
}

/// Pawns of `color` on the sixth rank or beyond, counted from their own side.
fn advanced_pawns(board: &Board, color: Color) -> i32 {
    (board.pawns() & board.by_color(color))
        .into_iter()
        .filter(|square| color.relative_rank(square.rank()).to_u32() >= 5)
        .count() as i32
}

/// Number of king moves between two squares.
fn king_distance(a: Square, b: Square) -> i32 {
    a.file().distance(b.file()).max(a.rank().distance(b.rank())) as i32
}

/// How far a square is from the four centre squares, in king moves along the
/// file plus along the rank. Zero in the centre, six in a corner.
fn centre_distance(square: Square) -> i32 {
    let file = square.file().to_u32() as i32;
    let rank = square.rank().to_u32() as i32;
    (file - 3).abs().min((file - 4).abs()) + (rank - 3).abs().min((rank - 4).abs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shakmaty::CastlingMode;
    use shakmaty::fen::Fen;

    fn position(fen: &str) -> Chess {
        fen.parse::<Fen>()
            .unwrap_or_else(|e| panic!("{fen}: {e}"))
            .into_position(CastlingMode::Standard)
            .unwrap_or_else(|e| panic!("{fen}: {e}"))
    }

    #[test]
    fn being_down_material_is_the_point() {
        let even = position("4k3/8/8/8/8/8/8/4K2R w - - 0 1");
        let down_a_rook = position("4k2r/8/8/8/8/8/8/4K3 w - - 0 1");
        let weights = Weights::material_only();
        assert_eq!(evaluate_with(&even, Color::White, &weights), -500);
        assert_eq!(evaluate_with(&down_a_rook, Color::White, &weights), 500);
        // And it is symmetric: what is good for one victim is bad for the other.
        assert_eq!(
            evaluate_with(&even, Color::White, &weights),
            -evaluate_with(&even, Color::Black, &weights)
        );
    }

    #[test]
    fn a_hanging_queen_scores_better_than_a_safe_one() {
        // White queen on d5 is attacked by the black king's neighbour... use a
        // clearer setup: black rook on d8 eyes the white queen on d5.
        let hanging = position("3rk3/8/8/3Q4/8/8/8/4K3 b - - 0 1");
        let safe = position("3rk3/8/8/4Q3/8/8/8/4K3 b - - 0 1");
        let weights = Weights {
            material: 100,
            hanging: 45,
            attacked: 12,
            ..Weights::zeroed()
        };
        assert!(
            evaluate_with(&hanging, Color::White, &weights)
                > evaluate_with(&safe, Color::White, &weights),
            "leaving the queen en prise should look more promising"
        );
    }

    #[test]
    fn trading_down_to_a_dead_draw_is_punished() {
        // White has given away everything, but so has black: a lone king cannot
        // mate, so there is no defeat left to be had here.
        let dead = position("4k3/8/8/8/8/8/8/4K3 w - - 0 1");
        // Black still has the queen it needs to finish white off.
        let losable = position("3qk3/8/8/8/8/8/8/4K3 w - - 0 1");
        let weights = Weights::default();
        assert!(
            evaluate_with(&dead, Color::White, &weights)
                < evaluate_with(&losable, Color::White, &weights),
            "giving away the opponent's mating material is worse than keeping it"
        );
    }

    #[test]
    fn winning_is_the_worst_outcome_of_all() {
        // Up a queen against a bare king: nobody can mate white, so the best
        // white can hope for is a draw. That must score below being mated.
        let winning = position("4k3/8/8/8/8/8/8/3QK3 w - - 0 1");
        let losing = position("3qk3/8/8/8/8/8/8/4K3 w - - 0 1");
        let weights = Weights::default();
        assert!(
            evaluate_with(&winning, Color::White, &weights)
                < evaluate_with(&losing, Color::White, &weights)
        );
    }

    #[test]
    fn an_exposed_king_is_an_opportunity() {
        // Black keeps a rook in both, since a king is only in danger from an
        // opponent who has something to threaten it with.
        let castled = position("3rk3/8/8/8/8/8/5PPP/6KR w - - 0 1");
        let wandering = position("3rk3/8/8/4K3/8/8/5PPP/7R w - - 0 1");
        let weights = Weights {
            king_ring_attack: 14,
            king_shelter: 25,
            king_advance: 22,
            king_centre: 14,
            ..Weights::zeroed()
        };
        assert!(
            evaluate_with(&wandering, Color::White, &weights)
                > evaluate_with(&castled, Color::White, &weights),
            "a king in the middle of the board is easier to catch"
        );
    }

    #[test]
    fn a_bare_king_wants_to_be_in_the_corner() {
        // Once there is nothing left to defend with, mate happens at the edge
        // of the board, so that is where the victim's king should be heading.
        // This is the opposite of the middlegame instinct above, which is why
        // the two are separate terms.
        let centre = position("8/8/8/4k3/8/8/8/3QK3 w - - 0 1");
        let corner = position("k7/8/8/8/8/8/8/3QK3 w - - 0 1");
        let weights = Weights::default();
        assert!(
            evaluate_with(&corner, Color::Black, &weights)
                > evaluate_with(&centre, Color::Black, &weights),
            "a cornered bare king is closer to being mated"
        );

        // And the mating king should be walking towards it.
        let far = position("k7/8/8/8/8/8/8/3QK3 w - - 0 1");
        let close = position("k7/8/1K6/8/8/8/8/3Q4 w - - 0 1");
        assert!(
            evaluate_with(&close, Color::Black, &weights)
                > evaluate_with(&far, Color::Black, &weights),
            "the other king coming up is what actually finishes the game"
        );
    }

    #[test]
    fn the_evaluation_is_antisymmetric() {
        // Swapping which player is trying to lose must flip the score, or the
        // malice setting could not turn this into an ordinary engine by
        // negating it. The draw-related terms are deliberately exempt, so this
        // uses a position where neither side is anywhere near a draw.
        for fen in [
            "3q1rk1/pp3ppp/8/8/8/6P1/PP3P1P/3Q1RK1 w - - 0 1",
            "r1bqkbnr/pppp1ppp/2n5/4p3/2B1P3/5Q2/PPPP1PPP/RNB1K1NR w KQkq - 4 4",
            "2r3k1/5ppp/8/3n4/8/2Q5/5PPP/6K1 b - - 0 1",
        ] {
            let position = position(fen);
            let weights = Weights::default();
            let white = evaluate_with(&position, Color::White, &weights);
            let black = evaluate_with(&position, Color::Black, &weights);
            assert_eq!(white, -black, "{fen} scored {white} and {black}");
        }
    }

    #[test]
    fn mating_material_is_measured_honestly() {
        let two_knights = position("4k3/8/8/8/8/8/8/1NNK4 w - - 0 1");
        assert!(!can_force_mate(two_knights.board(), Color::White));
        assert!(
            can_ever_mate(two_knights.board(), Color::White),
            "two knights cannot force mate but can still deliver one"
        );

        let lone_bishop = position("4k3/8/8/8/8/8/8/2BK4 w - - 0 1");
        assert!(!can_force_mate(lone_bishop.board(), Color::White));
        assert!(!can_ever_mate(lone_bishop.board(), Color::White));

        let bishop_and_knight = position("4k3/8/8/8/8/8/8/1NBK4 w - - 0 1");
        assert!(can_force_mate(bishop_and_knight.board(), Color::White));
    }

    #[test]
    fn attack_maps_cover_what_you_expect() {
        let start = Chess::default();
        let white = attack_map(start.board(), Color::White, start.board().occupied());
        assert!(white.contains(Square::E3), "pawn covers e3");
        assert!(white.contains(Square::A3), "knight covers a3");
        assert!(!white.contains(Square::E4), "a pawn push is not an attack");
    }
}
