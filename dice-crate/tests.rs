use super::*;

/// Roller that returns a scripted sequence of faces.
struct Fixed(std::vec::IntoIter<u32>);
fn fixed(faces: &[u32]) -> Fixed {
    Fixed(faces.to_vec().into_iter())
}
impl Roller for Fixed {
    fn roll(&mut self, _sides: u32) -> u32 {
        self.0.next().expect("script exhausted")
    }
}

/// Tiny deterministic LCG for statistical tests (no external crates).
struct Lcg(u64);
impl Roller for Lcg {
    fn roll(&mut self, sides: u32) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) % sides as u64) as u32 + 1
    }
}

fn value(input: &str, faces: &[u32]) -> i64 {
    parse(input).unwrap().eval(&mut fixed(faces)).unwrap().value
}

// ------------------------------------------------------------- parsing

#[test]
fn parses_basics() {
    assert!(parse("d20").is_ok());
    assert!(parse("3d6").is_ok());
    assert!(parse("d%").is_ok());
    assert!(parse("4dF").is_ok());
    assert!(parse("2d6+3").is_ok());
    assert!(parse("1d8 + 1d6 + 2").is_ok());
    assert!(parse("2d6*3-1").is_ok());
    assert!(parse("4d6kh3").is_ok());
    assert!(parse("4d6dl1").is_ok());
    assert!(parse("3d6!").is_ok());
    assert!(parse("10d6r2").is_ok());
    assert!(parse("10d6ro1").is_ok());
    assert!(parse("8d10>=7").is_ok());
    assert!(parse("d20adv").is_ok());
    assert!(parse("d20dis+5").is_ok());
    assert!(parse("10d6!>=5").is_ok());
}

#[test]
fn implicit_count_is_one() {
    let Expr(products) = parse("d20").unwrap();
    match &products[0].1 .0[0] {
        Term::Dice(dt) => assert_eq!(dt.count, 1),
        _ => panic!(),
    }
}

#[test]
fn percent_is_d100() {
    let Expr(products) = parse("d%").unwrap();
    match &products[0].1 .0[0] {
        Term::Dice(dt) => assert_eq!(dt.sides, Sides::Faces(100)),
        _ => panic!(),
    }
}

#[test]
fn adv_desugars_to_2kh1() {
    let Expr(products) = parse("d20adv").unwrap();
    match &products[0].1 .0[0] {
        Term::Dice(dt) => {
            assert_eq!(dt.count, 2);
            assert_eq!(dt.keep, Some((KeepKind::KeepHighest, 1)));
        }
        _ => panic!(),
    }
}

#[test]
fn suffix_writing_order_is_free() {
    // Suffixes apply in fixed semantic order regardless of writing order.
    assert_eq!(parse("4d6kh3!").unwrap(), parse("4d6!kh3").unwrap());
}

#[test]
fn parse_errors_have_positions() {
    let e = parse("2d").unwrap_err();
    assert_eq!(e.col, 3);
    assert!(e.msg.contains("die size"), "{}", e.msg);

    let e = parse("2d6 + ").unwrap_err();
    assert_eq!(e.col, 7);

    let e = parse("banana").unwrap_err();
    assert_eq!(e.col, 1);

    let e = parse("2d6 2d8").unwrap_err();
    assert_eq!(e.col, 5); // trailing input

    let e = parse("2 d6").unwrap_err(); // no whitespace within a dice term
    assert_eq!(e.col, 3);
}

#[test]
fn rejects_bad_dice() {
    assert!(parse("0d6").is_err());
    assert!(parse("d1").is_err());
    assert!(parse("d0").is_err());
    assert!(parse("101d6").is_err()); // MAX_DICE_PER_TERM
    assert!(parse("d10001").is_err()); // MAX_SIDES
    assert!(parse("4d6kh5").is_err()); // keep > count
    assert!(parse("4d6kh0").is_err());
    assert!(parse("2d20adv").is_err()); // adv needs a single die
    assert!(parse("d20kh1adv").is_err());
    assert!(parse("4dF!").is_err()); // no exploding Fate dice
    assert!(parse("4dFr1").is_err()); // no rerolling Fate dice
    assert!(parse("d6!!").is_err());
    assert!(parse("d6>=3>=4").is_err());
    assert!(parse("d6kh1kl1").is_err());
    assert!(parse("d6r1ro1").is_err());
    assert!(parse("").is_err());
    assert!(parse("d6ad").is_err()); // truncated 'adv'
    assert!(parse("2d6â‚¬").is_err()); // non-ASCII
}

#[test]
fn term_limit() {
    let twenty_one = vec!["1"; MAX_TERMS + 1].join("+");
    assert!(parse(&twenty_one).is_err());
    let twenty = vec!["1"; MAX_TERMS].join("+");
    assert!(parse(&twenty).is_ok());
}

// ---------------------------------------------------------- evaluation

#[test]
fn sums_and_arithmetic() {
    assert_eq!(value("2d6+3", &[4, 5]), 12);
    assert_eq!(value("2d6-1d4", &[4, 5, 2]), 7);
    assert_eq!(value("2+3", &[]), 5);
    // '*' binds tighter than '+': 1 + (2d6 * 3)
    assert_eq!(value("1+2d6*3", &[2, 3]), 16);
}

#[test]
fn fate_dice() {
    // Faces 1,2,3 map to −1,0,+1.
    assert_eq!(value("4dF", &[1, 2, 3, 3]), 1);
}

#[test]
fn keep_and_drop() {
    assert_eq!(value("4d6kh3", &[1, 4, 4, 6]), 14);
    assert_eq!(value("4d6kl1", &[3, 1, 5, 2]), 1);
    assert_eq!(value("4d6dl1", &[3, 1, 5, 2]), 10);
    assert_eq!(value("4d6dh2", &[3, 1, 5, 2]), 3);
    assert_eq!(value("d20adv", &[7, 18]), 18);
    assert_eq!(value("d20dis", &[7, 18]), 7);
}

#[test]
fn keep_ties_keep_pool_size() {
    // kh2 over [6,6,6,1]: exactly two dice kept.
    let out = parse("4d6kh2").unwrap().eval(&mut fixed(&[6, 6, 6, 1])).unwrap();
    match &out.products[0].1[0] {
        TermOutcome::Dice { dice, value, .. } => {
            assert_eq!(*value, 12);
            assert_eq!(dice.iter().filter(|d| !d.dropped).count(), 2);
        }
        _ => panic!(),
    }
}

#[test]
fn rerolls() {
    // ro1: each 1 rerolled once, replacement stands even if 1.
    assert_eq!(value("3d6ro1", &[1, 4, 1, 6, 1, 1]), 4 + 6 + 1);
    // r2: reroll until above 2.
    assert_eq!(value("2d6r2", &[2, 1, 5, 3]), 8);
    // r6 on a d6 can never terminate.
    assert_eq!(
        parse("d6r6").unwrap().eval(&mut fixed(&[])).unwrap_err(),
        EvalError::RerollUnsatisfiable
    );
}

#[test]
fn explosions() {
    // d6!: 6 explodes into 6 which explodes into 2.
    assert_eq!(value("d6!", &[6, 6, 2]), 14);
    // Non-max face doesn't explode.
    assert_eq!(value("d6!", &[3]), 3);
}

#[test]
fn explosion_depth_capped() {
    struct AlwaysMax;
    impl Roller for AlwaysMax {
        fn roll(&mut self, sides: u32) -> u32 {
            sides
        }
    }
    let out = parse("d6!").unwrap().eval(&mut AlwaysMax).unwrap();
    // Original die + at most MAX_EXPLOSION_DEPTH extras.
    assert_eq!(out.value, 6 * (1 + MAX_EXPLOSION_DEPTH as i64));
}

#[test]
fn explode_happens_before_drop() {
    // 2d6!dh1 with faces [6, 3] then explosion rolls [4]:
    // pool becomes [6, 4!, 3]; dropping the single highest removes the 6.
    assert_eq!(value("2d6!dh1", &[6, 3, 4]), 7);
}

#[test]
fn success_counting() {
    assert_eq!(value("5d10>=7", &[7, 2, 10, 6, 8]), 3);
    assert_eq!(value("5d10>7", &[7, 2, 10, 6, 8]), 2); // strict
    assert_eq!(value("5d10<=2", &[7, 2, 10, 6, 8]), 1);
    assert_eq!(value("5d10<2", &[7, 2, 10, 6, 8]), 0); // strict
    // Successes participate in arithmetic.
    assert_eq!(value("3d6>=5+2", &[5, 6, 1]), 4);
    // Success counts only kept dice.
    assert_eq!(value("4d6kh2>=5", &[5, 6, 1, 5]), 2);
}

#[test]
fn outcome_structure() {
    let out = parse("4d6kh3+2").unwrap().eval(&mut fixed(&[1, 4, 4, 6])).unwrap();
    assert_eq!(out.value, 16);
    assert_eq!(out.products.len(), 2);
    match &out.products[0].1[0] {
        TermOutcome::Dice { dice, value, .. } => {
            assert_eq!(*value, 14);
            assert_eq!(dice.len(), 4);
            let dropped: Vec<i64> =
                dice.iter().filter(|d| d.dropped).map(|d| d.value).collect();
            assert_eq!(dropped, vec![1]);
        }
        _ => panic!("expected dice term"),
    }
    assert_eq!(out.products[1].1[0], TermOutcome::Const(2));
}

#[test]
fn overflow_is_an_error() {
    // 100d10000 max is 1e6; chain multiplications to overflow i64.
    let expr = parse("100d10000*100d10000*100d10000*100d10000").unwrap();
    struct Max;
    impl Roller for Max {
        fn roll(&mut self, sides: u32) -> u32 {
            sides
        }
    }
    assert_eq!(expr.eval(&mut Max).unwrap_err(), EvalError::Overflow);
}

// ---------------------------------------------------------- statistical

#[test]
fn statistical_sanity() {
    let mut rng = Lcg(0xDECAFBAD);
    let expr = parse("3d6").unwrap();
    let n = 20_000;
    let mut sum = 0f64;
    for _ in 0..n {
        sum += expr.eval(&mut rng).unwrap().value as f64;
    }
    let mean = sum / n as f64;
    // E[3d6] = 10.5.
    assert!((mean - 10.5).abs() < 0.1, "mean was {mean}");

    // Advantage should beat a flat d20 (E ≈ 13.825 vs 10.5).
    let adv = parse("d20adv").unwrap();
    let mut sum = 0f64;
    for _ in 0..n {
        sum += adv.eval(&mut rng).unwrap().value as f64;
    }
    let mean = sum / n as f64;
    assert!((mean - 13.825).abs() < 0.2, "adv mean was {mean}");

    // d6r2 never yields faces ≤ 2 and its mean is (3+4+5+6)/4 = 4.5.
    let r = parse("d6r2").unwrap();
    let mut sum = 0f64;
    for _ in 0..n {
        let v = r.eval(&mut rng).unwrap().value;
        assert!(v >= 3);
        sum += v as f64;
    }
    let mean = sum / n as f64;
    assert!((mean - 4.5).abs() < 0.1, "r2 mean was {mean}");
}

#[test]
fn every_face_in_range() {
    let mut rng = Lcg(42);
    let expr = parse("10d8!r1").unwrap();
    for _ in 0..2_000 {
        let out = expr.eval(&mut rng).unwrap();
        for term in &out.products[0].1 {
            if let TermOutcome::Dice { dice, .. } = term {
                for die in dice {
                    assert!((1..=8).contains(&die.value));
                    if !die.exploded {
                        assert!(die.value >= 2, "r1 face leaked through");
                    }
                }
            }
        }
    }
}
