//! `POST /series/{namespace}/{database}/{series}/remote-read` — the
//! Prometheus remote-read half: a snappy-compressed `ReadRequest` answered
//! with a snappy-compressed `ReadResponse`, read from a declared series.
//!
//! # The mapping, stated once
//!
//! One `Query` becomes one read of the series: the millisecond range bounds
//! the declared `TIME` field, `__name__` matches the field `metric` and every
//! other matcher its own field, with `EQ` and `NEQ` answered exactly. A
//! regular-expression matcher is refused rather than approximated — the store
//! has no regex, and answering `RE` as `EQ` would return a plausible subset
//! with no signal. The read runs as the caller — their session, their grants,
//! field redaction included — exactly like the `/series` batch route.
//!
//! # The budget, and why it refuses rather than truncates
//!
//! A remote-read is a query Prometheus will happily aim at years of data, so
//! the read carries two declared ceilings on top of the caller's ordinary
//! ones: at most [`MOST_POINTS`] points and at most [`MOST_SERIES`] distinct
//! label sets per query. Past either the query is refused whole, naming the
//! ceiling — a truncated `QueryResult` is indistinguishable from a complete
//! one on the wire, and a monitoring system would alert on the silence the
//! truncation invented. On a cluster the read is an ordinary gathered read,
//! folded on the leaders under the same ceilings; nothing fans out that a
//! `SELECT` would not.
//!
//! # The wire, written by hand
//!
//! The `ReadRequest`/`ReadResponse` schema is a handful of small messages
//! (remote.proto: `Query` fields 1–3, `LabelMatcher` fields 1–3, `Sample` a
//! fixed64 double and a varint millisecond timestamp), so the codec here is a
//! walker and a writer over protobuf's two wire forms, like remote-write's —
//! no generated dependency.

use std::collections::BTreeMap;

use tessaridb::{Db, Parameters, Value};

use super::remote_write::Walker;
use super::{Answer, failure, session_for};
use crate::basic::Presented;
use crate::object::is_identifier;
use crate::tokens::Tokens;

/// The most points one query answers; past it the query is refused whole.
const MOST_POINTS: usize = 1_000_000;

/// The most distinct label sets one query answers; past it, refused whole.
const MOST_SERIES: usize = 10_000;

/// One `LabelMatcher`: 0 `=`, 1 `!=`; 2 and 3 are the regular expressions.
struct Matcher {
    kind: u64,
    name: String,
    value: String,
}

/// The label set a query groups by, and the points under it.
type Grouped = BTreeMap<Vec<(String, String)>, Vec<(i64, f64)>>;

/// One `Query` of the request.
struct Query {
    start: i64,
    end: i64,
    matchers: Vec<Matcher>,
}

/// Answer a remote-read body against the series `aimed` names.
pub(crate) async fn read(
    db: &Db,
    namespace: &str,
    database: &str,
    series: &str,
    body: &[u8],
    tokens: &Tokens,
    presented: &Presented,
) -> Answer {
    if ![namespace, database, series]
        .iter()
        .all(|name| is_identifier(name))
    {
        return Answer::bad_request(
            "a namespace, a database and a series are names: letters, digits and underscores",
        );
    }
    let Ok(mut inflated) = snap::raw::Decoder::new().decompress_vec(body) else {
        return Answer::bad_request("the body is snappy-compressed, and this did not decompress");
    };
    let queries = match read_request(&inflated) {
        Ok(queries) => queries,
        Err(reason) => return Answer::bad_request(&reason),
    };
    inflated.clear();
    let time = match db.is_series(namespace, database, series).await {
        Ok(true) => match db.series_time(namespace, database, series).await {
            Ok(Some(time)) => time,
            Ok(None) => {
                return Answer::bad_request(
                    "a series ordered by arrival time has no instant to read a range of",
                );
            }
            Err(error) => return failure(&error),
        },
        Ok(false) => {
            return Answer::new(
                404,
                format!(r#"{{"error":"no series named {series} in {namespace}.{database}"}}"#),
            );
        }
        Err(error) => return failure(&error),
    };
    let mut session = match session_for(db, tokens, presented).await {
        Ok(session) => session,
        Err(answer) => return answer,
    };
    if let Err(error) = session
        .run(&format!(
            "USE NAMESPACE {namespace}; USE DATABASE {database};"
        ))
        .await
    {
        return failure(&error);
    }
    let mut results = Vec::with_capacity(queries.len());
    for query in &queries {
        match answer_one(&mut session, series, &time, query).await {
            Ok(timeseries) => results.push(timeseries),
            Err(answer) => return answer,
        }
    }
    Answer::octets(200, snap_answer(&results))
}

/// One query's `TimeSeries` messages, grouped by the records' label sets.
async fn answer_one(
    session: &mut tessaridb::Session<'_>,
    series: &str,
    time: &str,
    query: &Query,
) -> Result<Vec<Vec<u8>>, Answer> {
    let mut condition = String::new();
    let mut given = Parameters::new();
    for (place, matcher) in query.matchers.iter().enumerate() {
        let field = if matcher.name == "__name__" {
            "metric"
        } else {
            matcher.name.as_str()
        };
        if !is_identifier(field) {
            return Err(Answer::bad_request(
                "a matcher names a field: letters, digits and underscores",
            ));
        }
        let op = match matcher.kind {
            0 => "=",
            1 => "!=",
            _ => {
                return Err(Answer::bad_request(
                    "regular-expression matchers are not answered — `=` and `!=` are, exactly",
                ));
            }
        };
        condition.push_str(&format!(" AND {field} {op} $m{place}"));
        given.insert(format!("m{place}"), Value::from(matcher.value.as_str()));
    }
    // The bounds are millisecond instants, bound as values so the range keeps
    // its sub-second edges exactly.
    let instant = |millis: i64| {
        let seconds = millis.div_euclid(1_000);
        let nanos = u32::try_from(millis.rem_euclid(1_000))
            .unwrap_or(0)
            .saturating_mul(1_000_000);
        tessaridb::Datetime::new(seconds, nanos)
            .unwrap_or_else(|| tessaridb::Datetime::from_seconds(seconds))
    };
    given.insert("from".to_owned(), Value::Datetime(instant(query.start)));
    given.insert("to".to_owned(), Value::Datetime(instant(query.end)));
    // One past the ceiling, so a full page and a truncated one are told apart:
    // the read that hits the bound refuses rather than answering a prefix.
    let script = format!(
        "SELECT * FROM {series} WHERE {time} >= $from AND {time} <= $to{condition} LIMIT {};",
        MOST_POINTS + 1
    );
    let outcomes = match session.run_with(&script, &given).await {
        Ok(outcomes) => outcomes,
        Err(error) => return Err(failure(&error)),
    };
    let Some(tessaridb::Outcome::Records { records, .. }) = outcomes.into_iter().last() else {
        return Err(Answer::new(
            500,
            r#"{"error":"the read answered no records"}"#.to_owned(),
        ));
    };
    if records.len() > MOST_POINTS {
        return Err(Answer::bad_request(
            "the query would answer more than 1000000 points — narrow the range or the matchers",
        ));
    }
    // Group into label sets: every string field but the value and the time
    // field is a label, `metric` first, the rest in name order.
    let mut grouped: Grouped = BTreeMap::new();
    for (_, record) in records {
        let Value::Object(fields) = record else {
            continue;
        };
        let mut labels: Vec<(String, String)> = Vec::new();
        let mut at = None;
        let mut value = None;
        for (name, held) in &fields {
            if name == time {
                if let Value::Datetime(instant) = held {
                    at = Some(
                        instant
                            .seconds()
                            .saturating_mul(1_000)
                            .saturating_add(i64::from(instant.nanos() / 1_000_000)),
                    );
                }
            } else if name == "value" {
                if let Value::Number(number) = held {
                    value = tessaridb::Number::float(0.0)
                        .as_float()
                        .and(number.as_float());
                }
            } else if let Value::String(text) = held {
                labels.push((name.clone(), text.clone()));
            }
        }
        labels.sort_by(|one, other| {
            (one.0 != "metric")
                .cmp(&(other.0 != "metric"))
                .then_with(|| one.0.cmp(&other.0))
        });
        let (Some(at), Some(value)) = (at, value) else {
            continue;
        };
        grouped.entry(labels).or_default().push((at, value));
    }
    if grouped.len() > MOST_SERIES {
        return Err(Answer::bad_request(
            "the query would answer more than 10000 series — narrow the range or the matchers",
        ));
    }
    Ok(grouped
        .into_iter()
        .map(|(labels, mut points)| {
            points.sort_by_key(|(at, _)| *at);
            time_series(&labels, &points)
        })
        .collect())
}

/// One `TimeSeries` message: its labels, then its samples in time order.
fn time_series(labels: &[(String, String)], points: &[(i64, f64)]) -> Vec<u8> {
    let mut message = Vec::new();
    for (name, value) in labels {
        let mut one = Vec::new();
        text_field(&mut one, 1, name);
        text_field(&mut one, 2, value);
        delimited(&mut message, 1, &one);
    }
    for (at, value) in points {
        let mut sample = Vec::with_capacity(10);
        sample.push((1 << 3) | 1);
        sample.extend_from_slice(&value.to_le_bytes());
        varint_field(&mut sample, 2, u64::try_from(*at).unwrap_or(0));
        delimited(&mut message, 2, &sample);
    }
    message
}

/// The `ReadResponse`: one `QueryResult` per query, each a list of series.
fn snap_answer(results: &[Vec<Vec<u8>>]) -> Vec<u8> {
    let mut response = Vec::new();
    for series in results {
        let mut result = Vec::new();
        for one in series {
            delimited(&mut result, 1, one);
        }
        delimited(&mut response, 1, &result);
    }
    snap::raw::Encoder::new()
        .compress_vec(&response)
        .unwrap_or_default()
}

/// A varint.
fn varint(into: &mut Vec<u8>, mut held: u64) {
    loop {
        let byte = u8::try_from(held & 0x7f).unwrap_or(0);
        held >>= 7;
        if held == 0 {
            into.push(byte);
            return;
        }
        into.push(byte | 0x80);
    }
}

/// A varint field.
fn varint_field(into: &mut Vec<u8>, field: u64, held: u64) {
    varint(into, field << 3);
    varint(into, held);
}

/// A length-delimited field.
fn delimited(into: &mut Vec<u8>, field: u64, held: &[u8]) {
    varint(into, (field << 3) | 2);
    varint(into, u64::try_from(held.len()).unwrap_or(0));
    into.extend_from_slice(held);
}

/// A string field.
fn text_field(into: &mut Vec<u8>, field: u64, held: &str) {
    delimited(into, field, held.as_bytes());
}

/// The `ReadRequest`, walked by hand: field 1 is a `Query`; a query holds its
/// range in fields 1 and 2 as varint milliseconds and a `LabelMatcher` per
/// field 3; a matcher is a varint kind and two strings.
fn read_request(bytes: &[u8]) -> Result<Vec<Query>, String> {
    let mut queries = Vec::new();
    let mut reader = Walker::new(bytes);
    while let Some((field, wire)) = reader.tag()? {
        if field == 1 && wire == 2 {
            queries.push(query(reader.delimited()?)?);
        } else {
            reader.skip(wire)?;
        }
    }
    Ok(queries)
}

/// One `Query`.
fn query(bytes: &[u8]) -> Result<Query, String> {
    let mut start = 0;
    let mut end = i64::MAX;
    let mut matchers = Vec::new();
    let mut reader = Walker::new(bytes);
    while let Some((field, wire)) = reader.tag()? {
        match (field, wire) {
            // protobuf `int64` is two's-complement in a varint, so the decode
            // is the bit reinterpretation, never a checked conversion.
            (1, 0) => start = reader.varint()?.cast_signed(),
            (2, 0) => end = reader.varint()?.cast_signed(),
            (3, 2) => matchers.push(matcher(reader.delimited()?)?),
            _ => reader.skip(wire)?,
        }
    }
    Ok(Query {
        start,
        end,
        matchers,
    })
}

/// One `LabelMatcher`.
fn matcher(bytes: &[u8]) -> Result<Matcher, String> {
    let mut kind = 0;
    let mut name = String::new();
    let mut value = String::new();
    let mut reader = Walker::new(bytes);
    while let Some((field, wire)) = reader.tag()? {
        match (field, wire) {
            (1, 0) => kind = reader.varint()?,
            (2, 2) => name = reader.text()?,
            (3, 2) => value = reader.text()?,
            _ => reader.skip(wire)?,
        }
    }
    Ok(Matcher { kind, name, value })
}

#[cfg(test)]
#[expect(
    clippy::panic,
    reason = "a unit test that cannot fail loudly proves nothing"
)]
mod tests {
    use super::{delimited, text_field, varint, varint_field};

    /// The writer and the reader agree on one matcher's bytes.
    #[test]
    fn a_written_matcher_reads_back() {
        let mut one = Vec::new();
        varint_field(&mut one, 1, 0);
        text_field(&mut one, 2, "__name__");
        text_field(&mut one, 3, "cpu_load");
        let mut query = Vec::new();
        varint_field(&mut query, 1, 0);
        delimited(&mut query, 3, &one);
        let mut request = Vec::new();
        delimited(&mut request, 1, &query);
        let Ok(queries) = super::read_request(&request) else {
            panic!("the request reads");
        };
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].matchers.len(), 1);
        assert_eq!(queries[0].matchers[0].name, "__name__");
        assert_eq!(queries[0].matchers[0].value, "cpu_load");
    }

    /// A varint round-trips at the boundaries.
    #[test]
    fn a_varint_carries_its_boundaries() {
        for held in [0_u64, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            let mut bytes = Vec::new();
            varint(&mut bytes, held);
            let mut reader = super::Walker::new(&bytes);
            assert_eq!(reader.varint().unwrap_or(u64::MAX), held);
        }
    }
}
