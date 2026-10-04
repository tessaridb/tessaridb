//! The written format and the code describe the same bytes (G059 C1).
//!
//! `docs/key-grammar.md` and `docs/value-system.md` are the normative statement
//! of the on-disk format and the code is its executable half. Nothing held the
//! two together: the doc's key-kind table stopped two kinds short of the code,
//! its index-value table two tags short, and its catalog section still counted
//! seven system tables when there were twenty-nine — each with every test green.
//!
//! So every table of the format is read from the doc and compared with the
//! code's own list both ways, and the code's list is itself probed for
//! completeness where the code can answer for every byte: a key-kind tag and a
//! payload tag over all 256 values, an index value and a record id over one
//! sample of every variant, chosen by a `match` with no wildcard so a new
//! variant does not compile until it is sampled.

use std::collections::BTreeSet;
use std::ops::Bound;

use tessari_encoding::{
    Error, Family, IndexValues, KeyKind, decode_payload, encode_record_id, format_surface,
};
use tessari_storage::SYSTEM_TABLES;
use tessari_types::{
    Datetime, Duration, Geometry, Number, RecordId, RecordRef, TableId, Value, ValueRange,
};

const KEY_GRAMMAR: &str = include_str!("../../../../docs/key-grammar.md");
const VALUE_SYSTEM: &str = include_str!("../../../../docs/value-system.md");

/// Where in the written format each family's table is, and which column holds
/// its names.
const TABLES: &[(Family, &str, &str, usize)] = &[
    (Family::KeyKind, KEY_GRAMMAR, "## 3. Key-kind tags", 1),
    (Family::Keyspace, KEY_GRAMMAR, "## 3. Key-kind tags", 2),
    (Family::RecordId, KEY_GRAMMAR, "## 5. `RecordId`", 1),
    (Family::ValueFlag, KEY_GRAMMAR, "### 7.1 Flag bits", 1),
    (
        Family::IndexValue,
        KEY_GRAMMAR,
        "## 7a. The index value tag table",
        1,
    ),
    (
        Family::Payload,
        VALUE_SYSTEM,
        "## 5. The payload encoding",
        1,
    ),
    (Family::NumberKind, VALUE_SYSTEM, "### 5.1 Number kinds", 1),
    (Family::Shape, VALUE_SYSTEM, "### 5.2 Geometry shapes", 1),
    (Family::BoundKind, VALUE_SYSTEM, "### 5.3 Range bounds", 1),
];

/// The rows of the first table under `heading`, up to the next heading: each
/// row's backticked codes (hex `0x..` or decimal) in its first column paired
/// with the backticked names in column `column`. A row naming nothing (the reserved `0x00`) is skipped.
fn rows(doc: &str, heading: &str, column: usize) -> Vec<(u32, String)> {
    let marker = format!("\n{heading}\n");
    let (_, body) = doc
        .split_once(&marker)
        .unwrap_or_else(|| panic!("no heading {heading:?} in the written format"));
    let end = body.find("\n#").unwrap_or(body.len());
    let ticked = |cell: &str| -> Vec<String> {
        cell.split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    };
    let mut found = Vec::new();
    for line in body[..end].lines().filter(|line| line.starts_with('|')) {
        let cells: Vec<&str> = line.split('|').skip(1).collect();
        let codes: Vec<u32> = ticked(cells[0])
            .iter()
            .filter_map(|code| match code.strip_prefix("0x") {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => code.parse().ok(),
            })
            .collect();
        let names = cells
            .get(column)
            .map(|cell| ticked(cell))
            .unwrap_or_default();
        if codes.is_empty() || names.is_empty() {
            continue;
        }
        assert_eq!(codes.len(), names.len(), "{heading}: {line}");
        found.extend(codes.into_iter().zip(names));
    }
    found
}

/// The variant spelling the doc uses for a key kind: `secondary-index` →
/// `SecondaryIndex`; a record id variant loses its payload: `Int(i64)` → `Int`.
fn spelled(family: Family, name: &str) -> String {
    match family {
        Family::KeyKind => name
            .split('-')
            .map(|part| {
                let mut letters = part.chars();
                letters
                    .next()
                    .map(|first| first.to_uppercase().chain(letters).collect::<String>())
                    .unwrap_or_default()
            })
            .collect(),
        _ => name.to_owned(),
    }
}

fn the_code_says(family: Family) -> BTreeSet<(u32, String)> {
    format_surface()
        .into_iter()
        .filter(|unit| unit.family == family)
        .map(|unit| (unit.code, spelled(family, unit.name)))
        .collect()
}

#[test]
fn every_table_of_the_written_format_holds_exactly_what_the_code_does() {
    let mut units = 0usize;
    for &(family, doc, heading, column) in TABLES {
        let written: BTreeSet<(u32, String)> = rows(doc, heading, column)
            .into_iter()
            .map(|(code, name)| (code, name.split('(').next().unwrap_or("").to_owned()))
            .collect();
        let code = the_code_says(family);
        let unwritten: Vec<_> = code.difference(&written).collect();
        let extra: Vec<_> = written.difference(&code).collect();
        assert!(
            unwritten.is_empty() && extra.is_empty(),
            "{family:?} under {heading:?}: in the code and not written {unwritten:x?}; \
             written and not in the code {extra:x?}"
        );
        units = units.saturating_add(code.len());
    }
    println!("[BGV_FIDELITY] format tables: source={units} | produced={units}");
}

#[test]
fn every_system_table_is_written_down_with_its_id() {
    let written: BTreeSet<(u32, String)> = rows(KEY_GRAMMAR, "### 9.1 The system tables", 1)
        .into_iter()
        .collect();
    let code: BTreeSet<(u32, String)> = SYSTEM_TABLES
        .iter()
        .map(|(id, name)| (id.get(), (*name).to_owned()))
        .collect();
    assert_eq!(written, code, "system tables: the doc against the code");
    assert_eq!(
        code.len(),
        SYSTEM_TABLES.len(),
        "a system table id is used twice"
    );
}

#[test]
fn the_written_codec_and_format_versions_are_the_ones_this_build_writes() {
    let codec = KEY_GRAMMAR
        .lines()
        .find(|line| line.starts_with("| `codec-version` |"))
        .and_then(|line| line.split('`').nth(3))
        .and_then(|hex| hex.strip_prefix("0x"))
        .and_then(|hex| u32::from_str_radix(hex, 16).ok());
    assert_eq!(
        codec,
        the_code_says(Family::Codec).first().map(|(code, _)| *code),
        "the codec version in §7"
    );
    let versions: Vec<u32> = rows(KEY_GRAMMAR, "## 10. Format versions", 0)
        .into_iter()
        .map(|(code, _)| code)
        .collect();
    let current = the_code_says(Family::Format)
        .first()
        .map(|(code, _)| *code)
        .expect("a current format");
    assert_eq!(
        versions,
        (1..=current).collect::<Vec<_>>(),
        "one row per format version"
    );
}

#[test]
fn the_code_lists_every_key_kind_and_payload_tag_it_decodes() {
    let kinds: BTreeSet<u32> = (0..=u8::MAX)
        .filter(|tag| KeyKind::from_tag(*tag).is_some())
        .map(u32::from)
        .collect();
    let listed: BTreeSet<u32> = the_code_says(Family::KeyKind)
        .iter()
        .map(|(code, _)| *code)
        .collect();
    assert_eq!(kinds, listed, "key kinds decoded against key kinds listed");

    let payloads: BTreeSet<u32> = (0..=u8::MAX)
        .filter(|tag| !matches!(decode_payload(&[*tag]), Err(Error::UnknownValueTag { .. })))
        .map(u32::from)
        .collect();
    let listed: BTreeSet<u32> = the_code_says(Family::Payload)
        .iter()
        .map(|(code, _)| *code)
        .collect();
    assert_eq!(
        payloads, listed,
        "payload tags decoded against payload tags listed"
    );
}

/// One value of every kind. A `match` with no wildcard, so a variant added to
/// `Value` does not compile here until it has a sample.
fn one_of_every_kind() -> Vec<Value> {
    let samples = vec![
        Value::None,
        Value::Null,
        Value::Bool(true),
        Value::Number(Number::Integer(1)),
        Value::String("a".to_owned()),
        Value::Bytes(vec![1]),
        Value::Duration(Duration::from_seconds(1)),
        Value::Datetime(Datetime::from_seconds(1)),
        Value::Uuid([1; 16]),
        Value::Table(TableId::new(1)),
        Value::Record(RecordRef::new(TableId::new(1), RecordId::Int(1))),
        Value::Array(Vec::new()),
        Value::Object(std::collections::BTreeMap::new()),
        Value::Range(Box::new(ValueRange::new(
            Bound::Unbounded,
            Bound::Unbounded,
        ))),
        Value::Set(BTreeSet::new()),
        Value::Geometry(Geometry::MultiPoint(Vec::new())),
        Value::Regex("a".to_owned()),
    ];
    for sample in &samples {
        match sample {
            Value::None
            | Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Bytes(_)
            | Value::Duration(_)
            | Value::Datetime(_)
            | Value::Uuid(_)
            | Value::Table(_)
            | Value::Record(_)
            | Value::Array(_)
            | Value::Object(_)
            | Value::Range(_)
            | Value::Set(_)
            | Value::Geometry(_)
            | Value::Regex(_) => {}
        }
    }
    samples
}

#[test]
fn every_value_and_record_id_encodes_under_a_listed_tag() {
    let led: BTreeSet<u32> = one_of_every_kind()
        .iter()
        .map(|value| u32::from(IndexValues::of(std::slice::from_ref(value)).as_slice()[0]))
        .collect();
    let listed: BTreeSet<u32> = the_code_says(Family::IndexValue)
        .iter()
        .map(|(code, _)| *code)
        .collect();
    assert_eq!(
        led, listed,
        "index value tags written against index value tags listed"
    );

    let ids = [
        RecordId::Int(1),
        RecordId::Text("a".to_owned()),
        RecordId::Uuid([1; 16]),
        RecordId::Bytes(vec![1]),
    ];
    for id in &ids {
        match id {
            RecordId::Int(_) | RecordId::Text(_) | RecordId::Uuid(_) | RecordId::Bytes(_) => {}
        }
    }
    let led: BTreeSet<u32> = ids
        .iter()
        .map(|id| u32::from(encode_record_id(id)[0]))
        .collect();
    let listed: BTreeSet<u32> = the_code_says(Family::RecordId)
        .iter()
        .map(|(code, _)| *code)
        .collect();
    assert_eq!(
        led, listed,
        "record id discriminants written against those listed"
    );
}
