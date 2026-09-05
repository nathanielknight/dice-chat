//! Dice expression parser and evaluator.
//!
//! Implements SPEC.md §8: a documented subset of de facto Roll20 notation.
//!
//! ```
//! use dice::{parse, Roller};
//! struct Fixed(Vec<u32>);
//! impl Roller for Fixed {
//!     fn roll(&mut self, _sides: u32) -> u32 { self.0.remove(0) }
//! }
//! let expr = parse("2d6+3").unwrap();
//! let out = expr.eval(&mut Fixed(vec![4, 5])).unwrap();
//! assert_eq!(out.value, 12);
//! ```

// ---------------------------------------------------------------- limits

pub const MAX_DICE_PER_TERM: u32 = 100;
pub const MAX_SIDES: u32 = 10_000;
pub const MAX_TERMS: usize = 20;
pub const MAX_EXPLOSION_DEPTH: u32 = 20;
/// Safety valve for `r` rerolls (termination is otherwise probabilistic).
const MAX_REROLLS_PER_DIE: u32 = 1_000;

// ------------------------------------------------------------------- AST

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AddOp {
    Add,
    Sub,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Sides {
    Faces(u32),
    /// Fate/Fudge die: −1, 0, +1.
    Fate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum KeepKind {
    KeepHighest,
    KeepLowest,
    DropHighest,
    DropLowest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RerollKind {
    /// `r n` — reroll faces ≤ n until above n.
    Indefinite,
    /// `ro n` — reroll each face ≤ n once.
    Once,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Cmp {
    Ge,
    Le,
    Gt,
    Lt,
}

impl Cmp {
    fn test(self, face: i64, target: i64) -> bool {
        match self {
            Cmp::Ge => face >= target,
            Cmp::Le => face <= target,
            Cmp::Gt => face > target,
            Cmp::Lt => face < target,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiceTerm {
    pub count: u32,
    pub sides: Sides,
    pub reroll: Option<(RerollKind, u32)>,
    pub explode: bool,
    pub keep: Option<(KeepKind, u32)>,
    pub success: Option<(Cmp, i64)>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Term {
    Const(i64),
    Dice(DiceTerm),
}

/// A product of terms: `t1 * t2 * …`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Product(pub Vec<Term>);

/// A parsed expression: sum/difference of products.
/// `*` binds tighter than `+`/`-`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Expr(pub Vec<(AddOp, Product)>);

// ---------------------------------------------------------------- errors

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub msg: String,
    /// 1-based column of the offending character.
    pub col: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at column {}", self.msg, self.col)
    }
}
impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    /// `r n` with n ≥ sides can never terminate.
    RerollUnsatisfiable,
    /// Exceeded [`MAX_REROLLS_PER_DIE`]; pathological roller.
    RerollLimit,
    Overflow,
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::RerollUnsatisfiable => {
                write!(f, "reroll threshold covers every face and can never stop")
            }
            EvalError::RerollLimit => write!(f, "reroll limit exceeded"),
            EvalError::Overflow => write!(f, "arithmetic overflow"),
        }
    }
}
impl std::error::Error for EvalError {}

// ---------------------------------------------------------------- parser
//
// Built on nom. Errors carry the remaining input at the point of failure
// (always a suffix of the full input), so the 1-based column is recovered
// at the top level as `full.len() - at.len() + 1`. Semantic errors use
// `Err::Failure` (or sit inside a `cut`) so `alt`/`many0` don't backtrack
// past them and lose the message.

use nom::branch::alt;
use nom::bytes::complete::tag;
use nom::character::complete::{char as chr, digit1};
use nom::combinator::{cut, map, opt, value};
use nom::multi::many0;
use nom::sequence::preceded;
use nom::{Err as NErr, IResult, Parser as _};

#[derive(Debug)]
struct PErr<'a> {
    /// Remaining input at the error (a suffix of the full input).
    at: &'a str,
    msg: String,
}

impl<'a> PErr<'a> {
    fn err(at: &'a str, msg: impl Into<String>) -> NErr<Self> {
        NErr::Error(PErr { at, msg: msg.into() })
    }

    fn fail(at: &'a str, msg: impl Into<String>) -> NErr<Self> {
        NErr::Failure(PErr { at, msg: msg.into() })
    }
}

impl<'a> nom::error::ParseError<&'a str> for PErr<'a> {
    fn from_error_kind(input: &'a str, _: nom::error::ErrorKind) -> Self {
        PErr { at: input, msg: "unexpected input".into() }
    }

    fn append(_: &'a str, _: nom::error::ErrorKind, other: Self) -> Self {
        other
    }
}

type PResult<'a, T> = IResult<&'a str, T, PErr<'a>>;

/// Parse a dice expression. See SPEC.md §8 for the grammar.
pub fn parse(input: &str) -> Result<Expr, ParseError> {
    if !input.is_ascii() {
        // Columns and keyword matching assume ASCII; the grammar is ASCII.
        let col = input
            .char_indices()
            .find(|(_, c)| !c.is_ascii())
            .map(|(i, _)| i + 1)
            .unwrap_or(1);
        return Err(ParseError { msg: "unexpected non-ASCII character".into(), col });
    }
    match expr(input) {
        Ok((rest, e)) => {
            let rest = rest.trim_start_matches([' ', '\t']);
            if rest.is_empty() {
                Ok(e)
            } else {
                Err(ParseError {
                    msg: "unexpected trailing input".into(),
                    col: input.len() - rest.len() + 1,
                })
            }
        }
        Err(NErr::Error(e)) | Err(NErr::Failure(e)) => {
            Err(ParseError { msg: e.msg, col: input.len() - e.at.len() + 1 })
        }
        Err(NErr::Incomplete(_)) => unreachable!("complete parsers only"),
    }
}

fn ws(i: &str) -> PResult<'_, ()> {
    Ok((i.trim_start_matches([' ', '\t']), ()))
}

fn integer<'a>(what: &'static str) -> impl FnMut(&'a str) -> PResult<'a, u64> {
    move |i| {
        let (rest, digits) =
            digit1(i).map_err(|_: NErr<PErr>| PErr::err(i, format!("expected {what}")))?;
        let n = digits
            .parse::<u64>()
            .map_err(|_| PErr::fail(i, format!("{what} is too large")))?;
        Ok((rest, n))
    }
}

// expr := product (("+" | "-") product)*
fn expr(i: &str) -> PResult<'_, Expr> {
    let (i, _) = ws(i)?;
    let (i, first) = product(i)?;
    let (i, more) = many0(|i| {
        let (i, _) = ws(i)?;
        let (i, op) = alt((value(AddOp::Add, chr('+')), value(AddOp::Sub, chr('-')))).parse(i)?;
        let (i, _) = ws(i)?;
        let (i, p) = cut(product).parse(i)?;
        Ok((i, (op, p)))
    })
    .parse(i)?;

    let mut out = vec![(AddOp::Add, first)];
    out.extend(more);
    let n: usize = out.iter().map(|(_, p)| p.0.len()).sum();
    if n > MAX_TERMS {
        return Err(PErr::fail(i, "too many terms in expression"));
    }
    Ok((i, Expr(out)))
}

// product := term ("*" term)*
fn product(i: &str) -> PResult<'_, Product> {
    let (i, first) = term(i)?;
    let (i, more) = many0(|i| {
        let (i, _) = ws(i)?;
        let (i, _) = chr('*').parse(i)?;
        let (i, _) = ws(i)?;
        cut(term).parse(i)
    })
    .parse(i)?;

    let mut out = vec![first];
    out.extend(more);
    if out.len() > MAX_TERMS {
        return Err(PErr::fail(i, "too many terms in expression"));
    }
    Ok((i, Product(out)))
}

// term := dice | integer
fn term(i: &str) -> PResult<'_, Term> {
    if i.is_empty() {
        return Err(PErr::err(i, "expected a number or dice term, found end of input"));
    }
    alt((dice_term, const_term)).parse(i).map_err(|e| match e {
        NErr::Error(_) => PErr::err(i, "expected a number or dice term"),
        other => other,
    })
}

fn const_term(i: &str) -> PResult<'_, Term> {
    let (rest, n) = integer("a number")(i)?;
    let n = i64::try_from(n).map_err(|_| PErr::fail(i, "number is too large"))?;
    Ok((rest, Term::Const(n)))
}

// dice := integer? "d" sides suffix*
fn dice_term(i: &str) -> PResult<'_, Term> {
    let (rest, count) = opt(integer("a number")).parse(i)?;
    let (rest, _) = chr('d').parse(rest)?;
    // Past the 'd' this is definitely a dice term: cut so errors surface
    // instead of backtracking into the constant alternative.
    let (rest, dt) = cut(move |r| dice_body(r, count, i)).parse(rest)?;
    Ok((rest, Term::Dice(dt)))
}

fn dice_body<'a>(i: &'a str, count: Option<u64>, term_start: &'a str) -> PResult<'a, DiceTerm> {
    let count = count.unwrap_or(1);
    if count == 0 {
        return Err(PErr::err(term_start, "dice count must be at least 1"));
    }
    if count > MAX_DICE_PER_TERM as u64 {
        return Err(PErr::err(
            term_start,
            format!("at most {MAX_DICE_PER_TERM} dice per term"),
        ));
    }

    let (i, sides) = sides(i)?;
    let (i, suffixes) = many0(suffix).parse(i)?;

    let mut term = DiceTerm {
        count: count as u32,
        sides,
        reroll: None,
        explode: false,
        keep: None,
        success: None,
    };
    for (at, s) in suffixes {
        apply_suffix(&mut term, at, s)?;
    }
    Ok((i, term))
}

fn sides(i: &str) -> PResult<'_, Sides> {
    alt((
        value(Sides::Fate, chr('F')),
        value(Sides::Faces(100), chr('%')),
        faces,
    ))
    .parse(i)
    .map_err(|e| match e {
        NErr::Error(_) => PErr::err(i, "expected die size after 'd' (a number, 'F', or '%')"),
        other => other,
    })
}

fn faces(i: &str) -> PResult<'_, Sides> {
    let (rest, n) = integer("die size")(i)?;
    if n < 2 {
        return Err(PErr::fail(i, "a die needs at least 2 sides"));
    }
    if n > MAX_SIDES as u64 {
        return Err(PErr::fail(i, format!("at most {MAX_SIDES} sides per die")));
    }
    Ok((rest, Sides::Faces(n as u32)))
}

/// A parsed-but-unvalidated suffix; validation happens in [`apply_suffix`]
/// where the whole term is in view.
#[derive(Debug, Clone, Copy)]
enum Suffix {
    Keep(KeepKind, u64),
    /// `adv`/`dis` sugar: d20adv ≡ 2d20kh1.
    AdvDis(KeepKind),
    Reroll(RerollKind, u64),
    Explode,
    Success(Cmp, u64),
}

fn suffix(i: &str) -> PResult<'_, (&str, Suffix)> {
    // Longest-match keyword order matters: "ro" before "r", ">=" before ">".
    // Numbers after a keyword are cut: "4d6kh" is an error, not trailing input.
    let (rest, s) = alt((
        map(preceded(tag("kh"), cut(integer("a count after keep/drop"))), |n| {
            Suffix::Keep(KeepKind::KeepHighest, n)
        }),
        map(preceded(tag("kl"), cut(integer("a count after keep/drop"))), |n| {
            Suffix::Keep(KeepKind::KeepLowest, n)
        }),
        map(preceded(tag("dh"), cut(integer("a count after keep/drop"))), |n| {
            Suffix::Keep(KeepKind::DropHighest, n)
        }),
        map(preceded(tag("dl"), cut(integer("a count after keep/drop"))), |n| {
            Suffix::Keep(KeepKind::DropLowest, n)
        }),
        value(Suffix::AdvDis(KeepKind::KeepHighest), tag("adv")),
        value(Suffix::AdvDis(KeepKind::KeepLowest), tag("dis")),
        map(preceded(tag("ro"), cut(integer("a threshold after reroll"))), |n| {
            Suffix::Reroll(RerollKind::Once, n)
        }),
        map(preceded(tag("r"), cut(integer("a threshold after reroll"))), |n| {
            Suffix::Reroll(RerollKind::Indefinite, n)
        }),
        value(Suffix::Explode, chr('!')),
        map(preceded(tag(">="), cut(integer("a target after the comparison"))), |n| {
            Suffix::Success(Cmp::Ge, n)
        }),
        map(preceded(tag("<="), cut(integer("a target after the comparison"))), |n| {
            Suffix::Success(Cmp::Le, n)
        }),
        map(preceded(tag(">"), cut(integer("a target after the comparison"))), |n| {
            Suffix::Success(Cmp::Gt, n)
        }),
        map(preceded(tag("<"), cut(integer("a target after the comparison"))), |n| {
            Suffix::Success(Cmp::Lt, n)
        }),
    ))
    .parse(i)?;
    Ok((rest, (i, s)))
}

/// Suffixes apply in writing order; a fixed semantic order governs
/// evaluation, so `4d6kh3!` and `4d6!kh3` produce the same term.
fn apply_suffix<'a>(term: &mut DiceTerm, at: &'a str, s: Suffix) -> Result<(), NErr<PErr<'a>>> {
    match s {
        Suffix::Keep(kind, n) => {
            if term.keep.is_some() {
                return Err(PErr::err(at, "only one keep/drop suffix per term"));
            }
            if n == 0 || n > term.count as u64 {
                return Err(PErr::err(
                    at,
                    format!("keep/drop count must be between 1 and {}", term.count),
                ));
            }
            term.keep = Some((kind, n as u32));
        }
        Suffix::AdvDis(kind) => {
            if term.count != 1 {
                return Err(PErr::err(
                    at,
                    "'adv'/'dis' only applies to a single die (e.g. d20adv)",
                ));
            }
            if term.keep.is_some() {
                return Err(PErr::err(at, "'adv'/'dis' cannot combine with keep/drop"));
            }
            term.count = 2;
            term.keep = Some((kind, 1));
        }
        Suffix::Reroll(kind, n) => {
            if term.sides == Sides::Fate {
                return Err(PErr::err(at, "Fate dice cannot be rerolled"));
            }
            if term.reroll.is_some() {
                return Err(PErr::err(at, "only one reroll suffix per term"));
            }
            let n = u32::try_from(n).map_err(|_| PErr::err(at, "reroll threshold is too large"))?;
            term.reroll = Some((kind, n));
        }
        Suffix::Explode => {
            if term.sides == Sides::Fate {
                return Err(PErr::err(at, "Fate dice cannot explode"));
            }
            if term.explode {
                return Err(PErr::err(at, "duplicate '!' suffix"));
            }
            term.explode = true;
        }
        Suffix::Success(cmp, n) => {
            if term.success.is_some() {
                return Err(PErr::err(at, "only one success comparison per term"));
            }
            let n = i64::try_from(n).map_err(|_| PErr::err(at, "success target is too large"))?;
            term.success = Some((cmp, n));
        }
    }
    Ok(())
}

// ------------------------------------------------------------ rendering

impl std::fmt::Display for Sides {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Sides::Fate => write!(f, "F"),
            Sides::Faces(n) => write!(f, "{n}"),
        }
    }
}

impl std::fmt::Display for Cmp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Cmp::Ge => ">=",
            Cmp::Le => "<=",
            Cmp::Gt => ">",
            Cmp::Lt => "<",
        })
    }
}

/// Canonical notation for a dice term (sugar like `adv` prints desugared).
impl std::fmt::Display for DiceTerm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}d{}", self.count, self.sides)?;
        if let Some((kind, n)) = self.reroll {
            let k = match kind {
                RerollKind::Indefinite => "r",
                RerollKind::Once => "ro",
            };
            write!(f, "{k}{n}")?;
        }
        if self.explode {
            write!(f, "!")?;
        }
        if let Some((kind, n)) = self.keep {
            let k = match kind {
                KeepKind::KeepHighest => "kh",
                KeepKind::KeepLowest => "kl",
                KeepKind::DropHighest => "dh",
                KeepKind::DropLowest => "dl",
            };
            write!(f, "{k}{n}")?;
        }
        if let Some((cmp, n)) = self.success {
            write!(f, "{cmp}{n}")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for Term {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Term::Const(n) => write!(f, "{n}"),
            Term::Dice(dt) => write!(f, "{dt}"),
        }
    }
}

impl std::fmt::Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, (op, Product(terms))) in self.0.iter().enumerate() {
            match (i, op) {
                (0, AddOp::Add) => {}
                (0, AddOp::Sub) => write!(f, "-")?,
                (_, AddOp::Add) => write!(f, " + ")?,
                (_, AddOp::Sub) => write!(f, " - ")?,
            }
            for (j, term) in terms.iter().enumerate() {
                if j > 0 {
                    write!(f, " * ")?;
                }
                write!(f, "{term}")?;
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------- evaluator

/// Source of randomness. `roll(sides)` must return a value in `1..=sides`.
pub trait Roller {
    fn roll(&mut self, sides: u32) -> u32;
}

/// One physical die in the outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Die {
    /// Final face value (−1..=1 for Fate dice).
    pub value: i64,
    /// Faces discarded by rerolls, in order.
    pub rerolled: Vec<i64>,
    /// This die was added by an explosion.
    pub exploded: bool,
    /// Discarded by a keep/drop suffix.
    pub dropped: bool,
    /// Passed the success comparison (only set when one is present).
    pub success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TermOutcome {
    Const(i64),
    Dice {
        term: DiceTerm,
        dice: Vec<Die>,
        /// Sum of kept faces, or success count if the term counts successes.
        value: i64,
    },
}

impl TermOutcome {
    pub fn value(&self) -> i64 {
        match self {
            TermOutcome::Const(n) => *n,
            TermOutcome::Dice { value, .. } => *value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Outcome {
    pub value: i64,
    pub products: Vec<(AddOp, Vec<TermOutcome>)>,
}

impl Expr {
    pub fn eval(&self, roller: &mut dyn Roller) -> Result<Outcome, EvalError> {
        let mut total: i64 = 0;
        let mut products = Vec::with_capacity(self.0.len());
        for (op, Product(terms)) in &self.0 {
            let mut outcomes = Vec::with_capacity(terms.len());
            let mut product: i64 = 1;
            for term in terms {
                let outcome = eval_term(term, roller)?;
                product = product
                    .checked_mul(outcome.value())
                    .ok_or(EvalError::Overflow)?;
                outcomes.push(outcome);
            }
            total = match op {
                AddOp::Add => total.checked_add(product),
                AddOp::Sub => total.checked_sub(product),
            }
            .ok_or(EvalError::Overflow)?;
            products.push((*op, outcomes));
        }
        Ok(Outcome { value: total, products })
    }
}

fn roll_face(sides: Sides, roller: &mut dyn Roller) -> i64 {
    match sides {
        // Map 1..=3 onto −1, 0, +1.
        Sides::Fate => roller.roll(3) as i64 - 2,
        Sides::Faces(n) => {
            let v = roller.roll(n);
            debug_assert!((1..=n).contains(&v), "Roller returned {v} for d{n}");
            v as i64
        }
    }
}

fn eval_term(term: &Term, roller: &mut dyn Roller) -> Result<TermOutcome, EvalError> {
    let dt = match term {
        Term::Const(n) => return Ok(TermOutcome::Const(*n)),
        Term::Dice(dt) => dt,
    };

    // Suffixes apply in fixed order: reroll → explode → keep/drop → success.

    // Reroll validation: `r n` where n covers every face can't terminate.
    if let (Some((RerollKind::Indefinite, n)), Sides::Faces(sides)) = (dt.reroll, dt.sides) {
        if n >= sides {
            return Err(EvalError::RerollUnsatisfiable);
        }
    }

    // 1. Initial pool + rerolls.
    let mut dice: Vec<Die> = Vec::with_capacity(dt.count as usize);
    for _ in 0..dt.count {
        let mut rerolled = Vec::new();
        let mut value = roll_face(dt.sides, roller);
        if let Some((kind, threshold)) = dt.reroll {
            let threshold = threshold as i64;
            match kind {
                RerollKind::Once => {
                    if value <= threshold {
                        rerolled.push(value);
                        value = roll_face(dt.sides, roller);
                    }
                }
                RerollKind::Indefinite => {
                    let mut spins = 0u32;
                    while value <= threshold {
                        spins += 1;
                        if spins > MAX_REROLLS_PER_DIE {
                            return Err(EvalError::RerollLimit);
                        }
                        rerolled.push(value);
                        value = roll_face(dt.sides, roller);
                    }
                }
            }
        }
        dice.push(Die { value, rerolled, exploded: false, dropped: false, success: false });
    }

    // 2. Explosions (rerolls don't apply to exploded dice).
    if dt.explode {
        if let Sides::Faces(sides) = dt.sides {
            let max = sides as i64;
            let mut i = 0;
            while i < dice.len() {
                if dice[i].value == max {
                    let mut depth = 0;
                    let mut insert_at = i + 1;
                    let mut face = max;
                    while face == max && depth < MAX_EXPLOSION_DEPTH {
                        face = roll_face(dt.sides, roller);
                        dice.insert(
                            insert_at,
                            Die {
                                value: face,
                                rerolled: Vec::new(),
                                exploded: true,
                                dropped: false,
                                success: false,
                            },
                        );
                        insert_at += 1;
                        depth += 1;
                    }
                    i = insert_at;
                } else {
                    i += 1;
                }
            }
        }
    }

    // 3. Keep/drop over the whole (post-explosion) pool.
    if let Some((kind, n)) = dt.keep {
        let n = (n as usize).min(dice.len());
        let mut order: Vec<usize> = (0..dice.len()).collect();
        // Sort ascending by face; ties broken by position (stable).
        order.sort_by_key(|&i| dice[i].value);
        let chosen: &[usize] = match kind {
            KeepKind::KeepHighest | KeepKind::DropHighest => &order[order.len() - n..],
            KeepKind::KeepLowest | KeepKind::DropLowest => &order[..n],
        };
        let keep_chosen = matches!(kind, KeepKind::KeepHighest | KeepKind::KeepLowest);
        let chosen: std::collections::HashSet<usize> = chosen.iter().copied().collect();
        for (i, die) in dice.iter_mut().enumerate() {
            die.dropped = chosen.contains(&i) != keep_chosen;
        }
    }

    // 4. Value: success count or sum over kept dice.
    let value = if let Some((cmp, target)) = dt.success {
        let mut count = 0i64;
        for die in dice.iter_mut() {
            if !die.dropped && cmp.test(die.value, target) {
                die.success = true;
                count += 1;
            }
        }
        count
    } else {
        let mut sum = 0i64;
        for die in dice.iter().filter(|d| !d.dropped) {
            sum = sum.checked_add(die.value).ok_or(EvalError::Overflow)?;
        }
        sum
    };

    Ok(TermOutcome::Dice { term: dt.clone(), dice, value })
}

#[cfg(test)]
mod tests;
