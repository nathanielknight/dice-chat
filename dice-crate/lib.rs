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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOp {
    Add,
    Sub,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sides {
    Faces(u32),
    /// Fate/Fudge die: −1, 0, +1.
    Fate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepKind {
    KeepHighest,
    KeepLowest,
    DropHighest,
    DropLowest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerollKind {
    /// `r n` — reroll faces ≤ n until above n.
    Indefinite,
    /// `ro n` — reroll each face ≤ n once.
    Once,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiceTerm {
    pub count: u32,
    pub sides: Sides,
    pub reroll: Option<(RerollKind, u32)>,
    pub explode: bool,
    pub keep: Option<(KeepKind, u32)>,
    pub success: Option<(Cmp, i64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Const(i64),
    Dice(DiceTerm),
}

/// A product of terms: `t1 * t2 * …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Product(pub Vec<Term>);

/// A parsed expression: sum/difference of products.
/// `*` binds tighter than `+`/`-`.
#[derive(Debug, Clone, PartialEq, Eq)]
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

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
}

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
    let mut p = Parser { src: input.as_bytes(), pos: 0 };
    let expr = p.expr()?;
    p.skip_ws();
    if p.pos < p.src.len() {
        return Err(p.err_here("unexpected trailing input"));
    }
    Ok(expr)
}

impl<'a> Parser<'a> {
    fn err_here(&self, msg: &str) -> ParseError {
        ParseError { msg: msg.into(), col: self.pos + 1 }
    }

    fn err_at(&self, msg: &str, pos: usize) -> ParseError {
        ParseError { msg: msg.into(), col: pos + 1 }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ') | Some(b'\t')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, s: &str) -> bool {
        if self.src[self.pos..].starts_with(s.as_bytes()) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn integer(&mut self, what: &str) -> Result<u64, ParseError> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start {
            return Err(self.err_here(&format!("expected {what}")));
        }
        std::str::from_utf8(&self.src[start..self.pos])
            .unwrap()
            .parse::<u64>()
            .map_err(|_| self.err_at(&format!("{what} is too large"), start))
    }

    // expr := product (("+" | "-") product)*
    fn expr(&mut self) -> Result<Expr, ParseError> {
        let mut out = Vec::new();
        self.skip_ws();
        out.push((AddOp::Add, self.product()?));
        loop {
            self.skip_ws();
            let op = match self.peek() {
                Some(b'+') => AddOp::Add,
                Some(b'-') => AddOp::Sub,
                _ => break,
            };
            self.pos += 1;
            self.skip_ws();
            out.push((op, self.product()?));
            let n: usize = out.iter().map(|(_, p)| p.0.len()).sum();
            if n > MAX_TERMS {
                return Err(self.err_here("too many terms in expression"));
            }
        }
        Ok(Expr(out))
    }

    // product := term ("*" term)*
    fn product(&mut self) -> Result<Product, ParseError> {
        let mut out = vec![self.term()?];
        loop {
            self.skip_ws();
            if self.peek() == Some(b'*') {
                self.pos += 1;
                self.skip_ws();
                out.push(self.term()?);
                if out.len() > MAX_TERMS {
                    return Err(self.err_here("too many terms in expression"));
                }
            } else {
                break;
            }
        }
        Ok(Product(out))
    }

    // term := dice | integer
    fn term(&mut self) -> Result<Term, ParseError> {
        let start = self.pos;
        match self.peek() {
            Some(b'd') => self.dice(1, start).map(Term::Dice),
            Some(c) if c.is_ascii_digit() => {
                let n = self.integer("a number")?;
                if self.peek() == Some(b'd') {
                    if n > MAX_DICE_PER_TERM as u64 {
                        return Err(self.err_at(
                            &format!("at most {MAX_DICE_PER_TERM} dice per term"),
                            start,
                        ));
                    }
                    self.dice(n as u32, start).map(Term::Dice)
                } else {
                    i64::try_from(n)
                        .map(Term::Const)
                        .map_err(|_| self.err_at("number is too large", start))
                }
            }
            Some(_) => Err(self.err_here("expected a number or dice term")),
            None => Err(self.err_here("expected a number or dice term, found end of input")),
        }
    }

    // dice := <count already parsed> "d" sides suffix*
    fn dice(&mut self, count: u32, term_start: usize) -> Result<DiceTerm, ParseError> {
        debug_assert_eq!(self.peek(), Some(b'd'));
        self.pos += 1; // consume 'd'

        if count == 0 {
            return Err(self.err_at("dice count must be at least 1", term_start));
        }

        let sides = match self.peek() {
            Some(b'F') => {
                self.pos += 1;
                Sides::Fate
            }
            Some(b'%') => {
                self.pos += 1;
                Sides::Faces(100)
            }
            Some(c) if c.is_ascii_digit() => {
                let sides_start = self.pos;
                let n = self.integer("die size")?;
                if n < 2 {
                    return Err(self.err_at("a die needs at least 2 sides", sides_start));
                }
                if n > MAX_SIDES as u64 {
                    return Err(self.err_at(
                        &format!("at most {MAX_SIDES} sides per die"),
                        sides_start,
                    ));
                }
                Sides::Faces(n as u32)
            }
            _ => return Err(self.err_here("expected die size after 'd' (a number, 'F', or '%')")),
        };

        let mut term = DiceTerm {
            count,
            sides,
            reroll: None,
            explode: false,
            keep: None,
            success: None,
        };

        loop {
            let suffix_start = self.pos;
            // Longest-match keyword order matters: "kh" before nothing else
            // starts with 'k'; "ro" before "r"; "dh"/"dl"/"dis" before bare 'd'
            // (a bare 'd' here is an error anyway — dice terms don't chain).
            if self.eat("kh") {
                self.keep_suffix(&mut term, KeepKind::KeepHighest, suffix_start)?;
            } else if self.eat("kl") {
                self.keep_suffix(&mut term, KeepKind::KeepLowest, suffix_start)?;
            } else if self.eat("dh") {
                self.keep_suffix(&mut term, KeepKind::DropHighest, suffix_start)?;
            } else if self.eat("dl") {
                self.keep_suffix(&mut term, KeepKind::DropLowest, suffix_start)?;
            } else if self.eat("adv") {
                self.advantage(&mut term, KeepKind::KeepHighest, suffix_start)?;
            } else if self.eat("dis") {
                self.advantage(&mut term, KeepKind::KeepLowest, suffix_start)?;
            } else if self.eat("ro") {
                self.reroll_suffix(&mut term, RerollKind::Once, suffix_start)?;
            } else if self.eat("r") {
                self.reroll_suffix(&mut term, RerollKind::Indefinite, suffix_start)?;
            } else if self.eat("!") {
                if term.sides == Sides::Fate {
                    return Err(self.err_at("Fate dice cannot explode", suffix_start));
                }
                if term.explode {
                    return Err(self.err_at("duplicate '!' suffix", suffix_start));
                }
                term.explode = true;
            } else if self.eat(">=") {
                self.success_suffix(&mut term, Cmp::Ge, suffix_start)?;
            } else if self.eat("<=") {
                self.success_suffix(&mut term, Cmp::Le, suffix_start)?;
            } else if self.eat(">") {
                self.success_suffix(&mut term, Cmp::Gt, suffix_start)?;
            } else if self.eat("<") {
                self.success_suffix(&mut term, Cmp::Lt, suffix_start)?;
            } else {
                break;
            }
        }

        Ok(term)
    }

    fn keep_suffix(
        &mut self,
        term: &mut DiceTerm,
        kind: KeepKind,
        at: usize,
    ) -> Result<(), ParseError> {
        if term.keep.is_some() {
            return Err(self.err_at("only one keep/drop suffix per term", at));
        }
        let n = self.integer("a count after keep/drop")?;
        if n == 0 || n > term.count as u64 {
            return Err(self.err_at(
                &format!("keep/drop count must be between 1 and {}", term.count),
                at,
            ));
        }
        term.keep = Some((kind, n as u32));
        Ok(())
    }

    fn advantage(
        &mut self,
        term: &mut DiceTerm,
        kind: KeepKind,
        at: usize,
    ) -> Result<(), ParseError> {
        // `adv`/`dis` is sugar valid only on a single die with no keep/drop:
        // d20adv ≡ 2d20kh1.
        if term.count != 1 {
            return Err(self.err_at("'adv'/'dis' only applies to a single die (e.g. d20adv)", at));
        }
        if term.keep.is_some() {
            return Err(self.err_at("'adv'/'dis' cannot combine with keep/drop", at));
        }
        term.count = 2;
        term.keep = Some((kind, 1));
        Ok(())
    }

    fn reroll_suffix(
        &mut self,
        term: &mut DiceTerm,
        kind: RerollKind,
        at: usize,
    ) -> Result<(), ParseError> {
        if term.sides == Sides::Fate {
            return Err(self.err_at("Fate dice cannot be rerolled", at));
        }
        if term.reroll.is_some() {
            return Err(self.err_at("only one reroll suffix per term", at));
        }
        let n = self.integer("a threshold after reroll")?;
        let n = u32::try_from(n).map_err(|_| self.err_at("reroll threshold is too large", at))?;
        term.reroll = Some((kind, n));
        Ok(())
    }

    fn success_suffix(
        &mut self,
        term: &mut DiceTerm,
        cmp: Cmp,
        at: usize,
    ) -> Result<(), ParseError> {
        if term.success.is_some() {
            return Err(self.err_at("only one success comparison per term", at));
        }
        let n = self.integer("a target after the comparison")?;
        let n = i64::try_from(n).map_err(|_| self.err_at("success target is too large", at))?;
        term.success = Some((cmp, n));
        Ok(())
    }
}

// ------------------------------------------------------------- evaluator

/// Source of randomness. `roll(sides)` must return a value in `1..=sides`.
pub trait Roller {
    fn roll(&mut self, sides: u32) -> u32;
}

/// One physical die in the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
