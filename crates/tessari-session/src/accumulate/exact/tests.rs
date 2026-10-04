//! The exact float total against an oracle that shares none of its arithmetic.
//!
//! The oracle is a fixed-point integer wide enough for every finite float — the
//! smallest subnormal is its unit — so it adds by integer carries and rounds by
//! reading bits. Agreeing with it means agreeing with the exact sum, which a
//! comparison against a second float algorithm could never show.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "a unit test that cannot fail loudly is no test"
)]

use proptest::prelude::*;

use super::ExactSum;
use super::oracle::{Wide, oracle};

fn exact(values: &[f64]) -> f64 {
    let mut total = ExactSum::default();
    for value in values {
        total.add(*value);
    }
    total.total().unwrap()
}

#[test]
fn the_total_is_the_exact_sum_rounded_once() {
    let cases: &[(&[f64], f64)] = &[
        (&[], 0.0),
        (&[0.1; 10], 1.0),
        (&[0.1, 0.2, 0.3], 0.6),
        (&[1e100, 1.0, -1e100, 1e-100, 1e50, -1.0, -1e50], 1e-100),
        (&[1e16, 1.0, -1e16], 1.0),
        (&[0.1, 0.2, 0.3, 1e16, -1e16], 0.6),
        // Halfway between 2^53 and its next float, then a nudge from far
        // below that decides which way it rounds.
        (
            &[9_007_199_254_740_992.0, 1.0, 1e-100],
            9_007_199_254_740_994.0,
        ),
        (
            &[9_007_199_254_740_992.0, 1.0, -1e-100],
            9_007_199_254_740_992.0,
        ),
        (&[9_007_199_254_740_992.0, 1.0], 9_007_199_254_740_992.0),
    ];
    for (values, expected) in cases {
        let answered = exact(values);
        assert_eq!(
            answered.to_bits(),
            expected.to_bits(),
            "{values:?}: {answered}"
        );
        assert_eq!(answered.to_bits(), oracle(values).to_bits(), "{values:?}");
    }
}

#[test]
fn infinities_add_as_ieee_adds_them_and_an_overflow_refuses() {
    let mut total = ExactSum::default();
    total.add(1.0);
    total.add(f64::INFINITY);
    assert_eq!(total.total(), Ok(f64::INFINITY));
    total.add(f64::NEG_INFINITY);
    assert!(total.total().unwrap().is_nan());

    let mut total = ExactSum::default();
    total.add(f64::MAX);
    total.add(f64::MAX);
    assert_eq!(total.total(), Err("a total outside the float range"));
}

#[test]
fn a_state_travels_and_comes_back_the_same_total() {
    let mut total = ExactSum::default();
    for value in [0.1, 1e300, -1e300, 3.5, 1e-310] {
        total.add(value);
    }
    let state = total.state().unwrap();
    let back = ExactSum::from_state(&state).unwrap();
    assert_eq!(back.total(), total.total());
    assert!(ExactSum::from_state(&tessari_types::Value::Bool(true)).is_none());
    // A state from another node is a trust boundary: a total with more parts
    // than any real one has is refused rather than squared.
    let many = tessari_types::Value::Array(vec![
        tessari_types::Value::Array(
            (0..65)
                .map(|at| tessari_types::Value::Number(tessari_types::Number::Float(f64::from(at))))
                .collect(),
        ),
        tessari_types::Value::None,
    ]);
    assert!(ExactSum::from_state(&many).is_none());
}

/// Floats across the whole range, with the hard cases over-represented: signs,
/// subnormals, values that cancel, and exponents far apart.
fn spread_float() -> impl Strategy<Value = f64> {
    prop_oneof![
        any::<f64>().prop_filter("finite", |value| value.is_finite() && value.abs() < 1e300),
        (-1e6_f64..1e6),
        (-1.0_f64..1.0).prop_map(|value| value * 1e-310),
        Just(0.1),
        Just(-0.1),
        Just(1e16),
        Just(-1e16),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn any_order_and_any_split_answer_the_oracles_bits(
        values in prop::collection::vec(spread_float(), 0..40),
        cut in 0_usize..40,
        seed in any::<u64>(),
    ) {
        let expected = oracle(&values);
        prop_assert_eq!(exact(&values).to_bits(), expected.to_bits());

        let mut shuffled = values.clone();
        // A fixed permutation from the seed, so a failure replays.
        let mut state = seed | 1;
        for at in (1..shuffled.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bound = u64::try_from(at.saturating_add(1)).unwrap();
            let other = usize::try_from(state.checked_rem(bound).unwrap()).unwrap();
            shuffled.swap(at, other);
        }
        prop_assert_eq!(exact(&shuffled).to_bits(), expected.to_bits());

        let cut = cut.min(values.len());
        let (left, right) = values.split_at(cut);
        let mut first = ExactSum::default();
        left.iter().for_each(|value| first.add(*value));
        let mut second = ExactSum::default();
        right.iter().for_each(|value| second.add(*value));
        let travelled = ExactSum::from_state(&second.state().unwrap()).unwrap();
        first.absorb(&travelled);
        prop_assert_eq!(first.total().unwrap().to_bits(), expected.to_bits());
    }

    #[test]
    fn a_scaled_or_squared_total_is_exact(
        values in prop::collection::vec(-1e6_f64..1e6, 1..12),
        by in 1_u32..1_000_000,
    ) {
        let mut total = ExactSum::default();
        values.iter().for_each(|value| total.add(*value));
        let by = f64::from(by);
        // Every value is below 2^20 in magnitude with 52 fraction bits, so a
        // product of two sits inside the oracle; the oracle multiplies by
        // adding, which shares nothing with a fused multiply-add.
        let scaled = total.scaled(by).total().unwrap();
        let mut wide = Wide::zero();
        for value in &values {
            let mut product = ExactSum::default();
            product.add_product(*value, by);
            // The exact product's two halves, added by integer carries.
            let held = product.state().unwrap();
            let tessari_types::Value::Array(held) = held else { panic!() };
            let tessari_types::Value::Array(parts) = &held[0] else { panic!() };
            for part in parts {
                let tessari_types::Value::Number(tessari_types::Number::Float(part)) = part
                else {
                    panic!()
                };
                wide.add(*part);
            }
        }
        prop_assert_eq!(scaled.to_bits(), wide.rounded().to_bits());
        let squared = total.squared().total().unwrap();
        let exactly = exact(&values);
        // Squaring the rounded total is the cross-check's floor: the exact
        // square differs from it by less than the rounding of the total itself.
        prop_assert!((squared - exactly * exactly).abs() <= (exactly * exactly).abs() * 1e-15 + 1e-300);
    }
}

/// ADR-0114's cost gate, run by hand in a release build:
/// `cargo test -p tessari-session --release --lib exact::tests::measure -- --ignored --nocapture`.
/// Prices with two decimals — what a `sum` over floats is usually asked of —
/// added in record order, against the exact total and the spread's two totals.
#[test]
#[ignore = "a measurement, read by hand in a release build"]
fn measure_the_cost_of_an_exact_total() {
    const VALUES: usize = 1_000_000;
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let values: Vec<f64> = (0..VALUES)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            f64::from(u32::try_from(state.checked_rem(1_000_000).unwrap()).unwrap()) / 100.0
        })
        .collect();
    let time = |what: &str, run: &dyn Fn() -> f64| {
        let mut best = f64::MAX;
        let mut answer = 0.0;
        for _ in 0..7 {
            let started = std::time::Instant::now();
            answer = std::hint::black_box(run());
            best = best.min(started.elapsed().as_secs_f64());
        }
        println!("{what}: {:.2} ns/value (answer {answer})", best * 1e9 / 1e6);
    };
    time("float in order", &|| {
        values.iter().fold(0.0, |held, value| held + value)
    });
    time("exact total", &|| {
        let mut total = ExactSum::default();
        values.iter().for_each(|value| total.add(*value));
        total.total().unwrap()
    });
    let numbers: Vec<tessari_types::Value> = values
        .iter()
        .map(|value| tessari_types::Value::Number(tessari_types::Number::Float(*value)))
        .collect();
    time("the sum fold, as built", &|| {
        let mut fold = crate::accumulate::Accumulator::for_aggregate(
            tessari_ql::Aggregate::Sum,
            tessari_ql::Span::new(0, 0),
        );
        numbers.iter().for_each(|value| fold.offer(value).unwrap());
        match fold.finish().unwrap() {
            tessari_types::Value::Number(tessari_types::Number::Float(total)) => total,
            _ => 0.0,
        }
    });
    time(
        "the sum fold before (exact decimal and a float in order)",
        &|| {
            let mut exact = Ok(rust_decimal::Decimal::ZERO);
            let mut float = 0.0_f64;
            numbers.iter().for_each(|value| {
                if let tessari_types::Value::Number(number) = value {
                    crate::accumulate::add_exact(&mut exact, number);
                    float += crate::aggregate::approximate(number).unwrap();
                }
            });
            float
        },
    );
    time("exact total and squares", &|| {
        let mut total = ExactSum::default();
        let mut squares = ExactSum::default();
        values.iter().for_each(|value| {
            total.add(*value);
            squares.add_product(*value, *value);
        });
        squares.total().unwrap() + total.total().unwrap()
    });
}

/// The spread's share of a whole read, by hand in a release build: the same
/// read with `count` (no arithmetic) and with `variance`, over one table.
#[test]
#[ignore = "a measurement, read by hand in a release build"]
fn measure_a_spread_inside_a_read() {
    use std::sync::Arc;
    let store = tessari_storage::Store::open(
        Arc::new(tessari_kv::MemoryBackend::new()) as Arc<dyn tessari_kv::KvBackend>
    )
    .unwrap();
    let mut session = crate::Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE COLLECTION r;",
        )
        .unwrap();
    let mut script = String::from("BEGIN;");
    for at in 0..200_000_u32 {
        script.push_str(&format!(
            "CREATE r = {{ x: {}.{:02} }};",
            at.checked_rem(10_000).unwrap(),
            at.checked_rem(100).unwrap()
        ));
    }
    script.push_str("COMMIT;");
    session.run(&script).unwrap();
    for read in [
        "SELECT count(x) AS n FROM r;",
        "SELECT sum(x) AS n FROM r;",
        "SELECT variance(x) AS n FROM r;",
    ] {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let started = std::time::Instant::now();
            std::hint::black_box(session.run(read).unwrap());
            best = best.min(started.elapsed().as_secs_f64());
        }
        println!("{read} {:.1} ns/record", best * 1e9 / 200_000.0);
    }
}
