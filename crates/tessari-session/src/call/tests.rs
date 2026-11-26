#![allow(clippy::panic)]

use tessari_ql::{Function, Span};
use tessari_types::{Number, Value};

use super::call;

fn at() -> Span {
    Span::new(0, 1)
}

fn text(value: &str) -> Value {
    Value::from(value)
}

/// Compare two answers by their written form rather than by `PartialEq`.
///
/// `Value`'s equality equates `Integer(3)`, `Float(3.0)` and
/// `Decimal("3.0")` deliberately, and the join relies on it. So an
/// assertion about which KIND a function answers cannot be written with
/// `assert_eq!` on the values themselves: it passes against a function
/// answering the other kind, and a kind is a difference the caller sees on
/// the wire (Q-76). The site that carries a message writes the comparison
/// out instead, so the message survives.
#[track_caller]
fn same(answered: impl core::fmt::Debug, expected: impl core::fmt::Debug) {
    assert_eq!(format!("{answered:?}"), format!("{expected:?}"));
}

#[test]
fn a_length_counts_characters_and_not_bytes() {
    let answer = call(Function::StringLen, &[text("héllo")], at()).expect("a length");
    same(answer, Value::Number(Number::Integer(5)));
}

#[test]
fn the_last_element_is_the_one_a_path_cannot_reach() {
    let items = Value::Array(vec![text("a"), text("b"), text("c")]);
    assert_eq!(
        call(Function::ArrayLast, &[items], at()).expect("an element"),
        text("c")
    );
}

#[test]
fn an_empty_array_answers_with_an_absence_rather_than_a_failure() {
    // An array having no first element is a state, not a mistake — the same
    // answer a route reaching nothing gives.
    let empty = Value::Array(Vec::new());
    assert_eq!(
        call(Function::ArrayFirst, &[empty], at()).expect("an answer"),
        Value::None
    );
}

#[test]
fn an_absent_argument_answers_with_an_absence_rather_than_failing() {
    // Without this a single record missing a field would fail a whole read,
    // and a store holding documents of differing shapes cannot have a
    // function surface that only works when every record has every field.
    assert_eq!(
        call(Function::StringLen, &[Value::None], at()).expect("an answer"),
        Value::None
    );
    assert_eq!(
        call(Function::ArrayLen, &[Value::Null], at()).expect("an answer"),
        Value::None
    );
    // Except for the one function that asks about the value itself.
    assert_eq!(
        call(Function::TypeOf, &[Value::None], at()).expect("an answer"),
        text("none")
    );
}

#[test]
fn a_wrong_argument_names_the_function_the_position_and_both_types() {
    let error = call(
        Function::StringLen,
        &[Value::Number(Number::Integer(1))],
        at(),
    )
    .expect_err("a refusal");
    let message = error.to_string();
    assert!(message.contains("string::len"), "{message}");
    assert!(message.contains("argument 1"), "{message}");
    assert!(message.contains("string"), "{message}");
    assert!(message.contains("number"), "{message}");
}

#[test]
fn rounding_keeps_the_kind_it_was_given() {
    let whole = Value::Number(Number::Integer(7));
    same(
        call(Function::MathFloor, std::slice::from_ref(&whole), at()).expect("a number"),
        &whole,
    );
    let fractional = Value::Number(Number::float(2.5));
    same(
        call(Function::MathRound, &[fractional], at()).expect("a number"),
        Value::Number(Number::float(3.0)),
    );
}

/// The instant every `time::` test below reads.
fn instant(text: &str) -> Value {
    Value::Datetime(tessari_types::Datetime::parse_rfc3339(text).expect("an instant"))
}

fn read(function: Function, value: Value) -> Value {
    call(function, &[value], at()).expect("a reading")
}

#[test]
fn each_reading_takes_its_own_field_and_not_a_neighbours() {
    // The failure this guards is a copy-paste one: six arms differing by a
    // single field name, where `time::minute` returning the hour is not a
    // crash and not a type error. The fixture has six distinct values so no
    // two fields can be swapped without the test noticing.
    let taken = instant("2026-08-28T14:37:09Z");
    for (function, expected) in [
        (Function::TimeYear, 2026),
        (Function::TimeMonth, 8),
        (Function::TimeDay, 28),
        (Function::TimeHour, 14),
        (Function::TimeMinute, 37),
        (Function::TimeSecond, 9),
    ] {
        assert_eq!(
            format!("{:?}", read(function, taken.clone())),
            format!("{:?}", Value::Number(Number::Integer(expected))),
            "{function}"
        );
    }
}

#[test]
fn the_second_of_the_minute_is_not_the_second_since_the_epoch() {
    // Two functions one letter apart in meaning, and both answer an integer,
    // so nothing but this asserts which is which.
    let taken = instant("2026-08-28T14:37:09Z");
    same(
        read(Function::TimeSecond, taken.clone()),
        Value::Number(Number::Integer(9)),
    );
    same(
        read(Function::TimeUnix, taken),
        Value::Number(Number::Integer(1_787_927_829)),
    );
}

#[test]
fn an_instant_survives_a_round_trip_through_its_second_count() {
    let taken = instant("2026-08-28T14:37:09Z");
    let seconds = read(Function::TimeUnix, taken.clone());
    assert_eq!(read(Function::TimeFromUnix, seconds), taken);
}

#[test]
fn a_sub_second_remainder_is_dropped_by_the_second_count_and_not_refused() {
    // Deliberate, and the reason is in `time::unix`'s comment: there is no
    // `time::unix_millis` to write instead, and `time::now()` carries a
    // remainder nearly always. Asserted so the loss is a decision on the
    // record rather than something nobody looked at.
    let taken = instant("2026-08-28T14:37:09.5Z");
    same(
        read(Function::TimeUnix, taken),
        Value::Number(Number::Integer(1_787_927_829)),
    );
}

#[test]
fn a_fractional_second_count_is_refused_rather_than_truncated() {
    let error = call(
        Function::TimeFromUnix,
        &[Value::Number(Number::float(1.5))],
        at(),
    )
    .expect_err("a refusal");
    let message = error.to_string();
    assert!(message.contains("time::from_unix"), "{message}");
    assert!(message.contains("math::round"), "{message}");
    // A whole number written as a float is not a fraction, and is taken.
    assert_eq!(
        read(Function::TimeFromUnix, Value::Number(Number::float(0.0))),
        Value::Datetime(tessari_types::Datetime::from_seconds(0))
    );
}

#[test]
fn a_reading_of_an_absent_instant_is_an_absence_and_narrows_the_read() {
    // None of the eight join `answers_for_absence`: there is no year that a
    // missing field has. The general rule therefore applies, and a record
    // without the field drops out of the read instead of failing it.
    assert_eq!(
        call(Function::TimeYear, &[Value::None], at()).expect("an answer"),
        Value::None
    );
}

/// The epoch second count of a civil midnight, stated as a constant so the
/// expectation owes nothing to the code under test.
fn midnight(seconds: i64) -> Value {
    Value::Datetime(tessari_types::Datetime::from_seconds(seconds))
}

#[test]
fn the_epoch_is_a_thursday_and_the_first_day_of_its_year() {
    // 1970-01-01: ISO day-of-week 4, day 1 of the year, ISO week 1, quarter 1.
    let epoch = midnight(0);
    same(
        call(Function::TimeDayOfWeek, std::slice::from_ref(&epoch), at()).expect("a day of week"),
        Value::Number(Number::Integer(4)),
    );
    same(
        call(Function::TimeDayOfYear, std::slice::from_ref(&epoch), at()).expect("a day of year"),
        Value::Number(Number::Integer(1)),
    );
    same(
        call(Function::TimeWeek, std::slice::from_ref(&epoch), at()).expect("a week"),
        Value::Number(Number::Integer(1)),
    );
    same(
        call(Function::TimeQuarter, &[epoch], at()).expect("a quarter"),
        Value::Number(Number::Integer(1)),
    );
}

#[test]
fn a_leap_day_reads_as_its_own_day_of_the_year() {
    // 2000-02-29 was a Tuesday and is day 60 of a leap year.
    let leap = midnight(951_782_400);
    same(
        call(Function::TimeDayOfWeek, std::slice::from_ref(&leap), at()).expect("a day of week"),
        Value::Number(Number::Integer(2)),
    );
    same(
        call(Function::TimeDayOfYear, std::slice::from_ref(&leap), at()).expect("a day of year"),
        Value::Number(Number::Integer(60)),
    );
    same(
        call(Function::TimeQuarter, &[leap], at()).expect("a quarter"),
        Value::Number(Number::Integer(1)),
    );
}

#[test]
fn an_iso_week_belongs_to_the_year_holding_its_thursday() {
    // 2019-12-30 is a Monday of ISO week 1 of 2020; 2021-01-01 is a Friday of
    // ISO week 53 of 2020 — the two boundary shapes the naive week-of-year
    // gets wrong.
    same(
        call(Function::TimeWeek, &[midnight(1_577_664_000)], at()).expect("a week"),
        Value::Number(Number::Integer(1)),
    );
    same(
        call(Function::TimeWeek, &[midnight(1_609_459_200)], at()).expect("a week"),
        Value::Number(Number::Integer(53)),
    );
}

#[test]
fn an_ordinary_day_reads_its_iso_week() {
    // 2026-11-26 is a Thursday, day 330 of 2026, in ISO week 48.
    same(
        call(Function::TimeWeek, &[midnight(1_795_651_200)], at()).expect("a week"),
        Value::Number(Number::Integer(48)),
    );
    same(
        call(Function::TimeDayOfYear, &[midnight(1_795_651_200)], at()).expect("a day of year"),
        Value::Number(Number::Integer(330)),
    );
}

#[test]
fn the_start_of_a_week_is_its_monday() {
    // 2026-11-26 is a Thursday; its week's Monday is 2026-11-23.
    same(
        call(Function::TimeStartOfWeek, &[midnight(1_795_651_200)], at()).expect("a start"),
        midnight(1_795_392_000),
    );
}

#[test]
fn the_starts_of_month_quarter_and_year_are_midnights_of_their_first_day() {
    let day = midnight(1_795_651_200);
    same(
        call(Function::TimeStartOfMonth, std::slice::from_ref(&day), at()).expect("a start"),
        midnight(1_793_491_200),
    );
    same(
        call(
            Function::TimeStartOfQuarter,
            std::slice::from_ref(&day),
            at(),
        )
        .expect("a start"),
        midnight(1_790_812_800),
    );
    same(
        call(Function::TimeStartOfYear, &[day], at()).expect("a start"),
        midnight(1_767_225_600),
    );
}

#[test]
fn a_start_keeps_no_sub_second_remainder_and_reads_an_absence_as_an_absence() {
    // A start is a whole second by construction; the remainder is dropped the
    // way `time::bucket`'s is, and an absent instant narrows the read.
    let with_nanos = Value::Datetime(
        tessari_types::Datetime::new(1_795_651_200, 999_999_999).expect("a normalised pair"),
    );
    same(
        call(Function::TimeStartOfDay, &[with_nanos], at()).expect("a start"),
        midnight(1_795_651_200),
    );
    assert_eq!(
        call(Function::TimeStartOfMonth, &[Value::None], at()).expect("an answer"),
        Value::None
    );
}

#[test]
fn tukey_names_the_indices_outside_the_fences() {
    // Quartiles of 1..=8 sit at 2.75 and 6.25; the fences are -2.5 and 11.5,
    // so only the 40 falls outside.
    let values = Value::Array(vec![
        integer_value(1),
        integer_value(2),
        integer_value(3),
        integer_value(4),
        integer_value(5),
        integer_value(6),
        integer_value(7),
        integer_value(8),
        integer_value(40),
    ]);
    same(
        call(Function::SeriesOutliersTukey, &[values], at()).expect("indices"),
        Value::Array(vec![Value::Number(Number::Integer(8))]),
    );
    // Nothing outside the fences is an empty array, not an absence.
    let calm = Value::Array(vec![integer_value(1), integer_value(2), integer_value(3)]);
    same(
        call(Function::SeriesOutliersTukey, &[calm], at()).expect("indices"),
        Value::Array(vec![]),
    );
}

#[test]
fn the_period_is_where_the_series_repeats() {
    // A sawtooth of period 4 over 24 samples.
    let mut wave = Vec::new();
    for step in 0..24 {
        wave.push(integer_value([0, 1, 2, 3][step % 4]));
    }
    let answered =
        call(Function::SeriesPeriodDetect, &[Value::Array(wave)], at()).expect("a period");
    let Value::Number(held) = answered else {
        panic!("a number")
    };
    let Some(period) = crate::aggregate::approximate(&held) else {
        panic!("a float")
    };
    assert!((period - 4.0).abs() < 0.5, "read {period}");
    // Too short to say is an absence.
    assert_eq!(
        call(
            Function::SeriesPeriodDetect,
            &[Value::Array(vec![integer_value(1), integer_value(2)])],
            at(),
        )
        .expect("an answer"),
        Value::None
    );
}

#[test]
fn the_decomposition_adds_back_up_to_the_series() {
    // trend + seasonal + residual is the series, wherever all three are
    // defined — the property the parts exist to have.
    let mut wave = Vec::new();
    for step in 0..24_i64 {
        wave.push(integer_value(
            step / 4 + [0, 2, 5, 2][usize::try_from(step % 4).expect("0..4")],
        ));
    }
    let answered = call(
        Function::SeriesDecomposeStl,
        &[
            Value::Array(wave.clone()),
            Value::Number(Number::Integer(4)),
        ],
        at(),
    )
    .expect("a decomposition");
    let Value::Object(parts) = answered else {
        panic!("an object")
    };
    let (Some(Value::Array(trend)), Some(Value::Array(seasonal)), Some(Value::Array(residual))) = (
        parts.get("trend"),
        parts.get("seasonal"),
        parts.get("residual"),
    ) else {
        panic!("three parts")
    };
    assert_eq!(trend.len(), wave.len());
    for (place, original) in wave.iter().enumerate() {
        let (Some(t), Some(s), Some(r)) = (
            trend.get(place).and_then(number_as_float),
            seasonal.get(place).and_then(number_as_float),
            residual.get(place).and_then(number_as_float),
        ) else {
            continue; // the trend's undefined edges hold none
        };
        let Value::Number(original) = original else {
            panic!("a number")
        };
        let whole = crate::aggregate::approximate(original).expect("a float");
        assert!(
            (t + s + r - whole).abs() < 1e-9,
            "at {place}: {t} + {s} + {r} != {whole}"
        );
    }
    // A period under two is refused by name.
    let refused = call(
        Function::SeriesDecomposeStl,
        &[Value::Array(wave), Value::Number(Number::Integer(1))],
        at(),
    );
    assert!(refused.is_err());
}

fn integer_value(held: i64) -> Value {
    Value::Number(Number::Integer(held))
}

fn number_as_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(held) => crate::aggregate::approximate(held),
        _ => None,
    }
}

#[test]
fn the_new_math_readings_answer_their_domains() {
    same(
        call(Function::MathLog2, &[integer_value(8)], at()).expect("a log"),
        Value::Number(Number::float(3.0)),
    );
    assert_eq!(
        call(Function::MathLog10, &[integer_value(0)], at()).expect("an answer"),
        Value::None
    );
    same(
        call(Function::MathCbrt, &[integer_value(-27)], at()).expect("a root"),
        Value::Number(Number::float(-3.0)),
    );
    same(
        call(
            Function::MathAtan2,
            &[integer_value(1), integer_value(1)],
            at(),
        )
        .expect("an angle"),
        Value::Number(Number::float(std::f64::consts::FRAC_PI_4)),
    );
    same(
        call(
            Function::MathHypot,
            &[integer_value(3), integer_value(4)],
            at(),
        )
        .expect("a side"),
        Value::Number(Number::float(5.0)),
    );
    assert_eq!(
        call(Function::MathAsin, &[integer_value(2)], at()).expect("an answer"),
        Value::None
    );
    same(
        call(Function::MathPi, &[], at()).expect("pi"),
        Value::Number(Number::float(std::f64::consts::PI)),
    );
    let degrees = call(
        Function::MathDegrees,
        &[Value::Number(Number::float(std::f64::consts::PI))],
        at(),
    )
    .expect("degrees");
    same(degrees, Value::Number(Number::float(180.0)));
}

#[test]
fn the_new_text_readings_repeat_pad_and_measure() {
    same(
        call(
            Function::StringRepeat,
            &[text("ab"), integer_value(3)],
            at(),
        )
        .expect("a repetition"),
        text("ababab"),
    );
    assert!(
        call(
            Function::StringRepeat,
            &[text("x"), integer_value(-1)],
            at()
        )
        .is_err()
    );
    same(
        call(
            Function::StringPadLeft,
            &[text("7"), integer_value(3), text("0")],
            at(),
        )
        .expect("a padding"),
        text("007"),
    );
    same(
        call(
            Function::StringPadRight,
            &[text("7"), integer_value(3), text(".")],
            at(),
        )
        .expect("a padding"),
        text("7.."),
    );
    assert!(
        call(
            Function::StringPadLeft,
            &[text("7"), integer_value(3), text("ab")],
            at()
        )
        .is_err()
    );
    same(
        call(
            Function::StringEditDistance,
            &[text("kitten"), text("sitting")],
            at(),
        )
        .expect("a distance"),
        integer_value(3),
    );
    // Characters, not bytes: one edit apart, not two.
    same(
        call(
            Function::StringEditDistance,
            &[text("héllo"), text("hello")],
            at(),
        )
        .expect("a distance"),
        integer_value(1),
    );
}

#[test]
fn the_new_array_readings_compact_count_and_range() {
    let mixed = Value::Array(vec![
        integer_value(1),
        Value::None,
        integer_value(1),
        integer_value(2),
    ]);
    same(
        call(Function::ArrayCompact, std::slice::from_ref(&mixed), at()).expect("a compacting"),
        Value::Array(vec![integer_value(1), integer_value(1), integer_value(2)]),
    );
    same(
        call(Function::ArrayCount, &[mixed, integer_value(1)], at()).expect("a count"),
        integer_value(2),
    );
    same(
        call(
            Function::ArrayRange,
            &[integer_value(2), integer_value(6)],
            at(),
        )
        .expect("a range"),
        Value::Array(vec![
            integer_value(2),
            integer_value(3),
            integer_value(4),
            integer_value(5),
        ]),
    );
    same(
        call(
            Function::ArrayRange,
            &[integer_value(5), integer_value(5)],
            at(),
        )
        .expect("a range"),
        Value::Array(vec![]),
    );
}

#[test]
fn a_version_seven_identifier_names_its_instant_and_another_version_names_none() {
    let minted = call(Function::RandUuid7, &[], at()).expect("an identifier");
    let Value::Uuid(bytes) = minted else {
        panic!("a uuid")
    };
    assert_eq!(bytes[6] >> 4, 7);
    let answered = call(Function::UuidInstant, &[Value::Uuid(bytes)], at()).expect("an instant");
    assert!(matches!(answered, Value::Datetime(_)), "{answered:?}");
    let four = call(Function::RandUuid, &[], at()).expect("an identifier");
    assert_eq!(
        call(Function::UuidInstant, &[four], at()).expect("an answer"),
        Value::None
    );
}
