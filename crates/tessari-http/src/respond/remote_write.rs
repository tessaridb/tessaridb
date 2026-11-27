//! `POST /series/{namespace}/{database}/{series}/remote-write` — the
//! Prometheus remote-write on-ramp: a snappy-compressed `WriteRequest` whose
//! samples land in a declared series.
//!
//! # The mapping, stated once
//!
//! Each `TimeSeries` becomes one record per sample: the `__name__` label is
//! the field `metric`, every other label a field of its own name, the sample's
//! value the field `value`, and its timestamp — milliseconds since the epoch,
//! as Prometheus sends them — the series' declared `TIME` field. A series
//! ordered by arrival time is refused by name: its samples' instants ARE the
//! data, and landing them nowhere would lose them silently. A label a strict
//! series does not declare is refused by the series' own rule, in its own
//! words — this route adds no second schema.
//!
//! # The wire, read by hand
//!
//! The `WriteRequest` schema is three small messages, so the decoder here is a
//! walker over protobuf's two wire forms (varint, length-delimited) rather
//! than a generated dependency; the snappy framing is the `snap` crate the
//! workspace already carries for Kafka. Anything the walker does not
//! recognise is a `400` naming what it met — a body that half-decodes is
//! refused whole, never appended in part.

use std::collections::BTreeMap;

use tessaridb::{Db, Parameters, Value};

use super::{Answer, failure, session_for};
use crate::basic::Presented;
use crate::object::is_identifier;
use crate::tokens::Tokens;

/// The samples one request carried, decoded and ready to map.
struct Sample {
    labels: Vec<(String, String)>,
    value: f64,
    /// Milliseconds since the epoch, as the wire carries them.
    at: i64,
}

/// Append a remote-write body to the series `aimed` names.
pub(crate) async fn append(
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
    let samples = match write_request(&inflated) {
        Ok(samples) => samples,
        Err(reason) => return Answer::bad_request(&reason),
    };
    inflated.clear();
    let time = match db.is_series(namespace, database, series).await {
        Ok(true) => match db.series_time(namespace, database, series).await {
            Ok(Some(time)) => time,
            Ok(None) => {
                return Answer::bad_request(
                    "a series ordered by arrival time cannot take remote-write samples: \
                     their instants are the data, and there is no field to land them in",
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
    let count = samples.len();
    let mut script = String::from("BEGIN;");
    let mut given = Parameters::new();
    for (position, sample) in samples.into_iter().enumerate() {
        script.push_str(&format!(" CREATE {series} = $s{position};"));
        given.insert(format!("s{position}"), record(sample, &time));
    }
    script.push_str(" COMMIT;");
    match session.run_with(&script, &given).await {
        Ok(_) => Answer::new(200, format!(r#"{{"appended":{count}}}"#)),
        Err(error) => failure(&error),
    }
}

/// One sample as the record the series stores.
fn record(sample: Sample, time: &str) -> Value {
    let mut held = BTreeMap::new();
    for (name, value) in sample.labels {
        let field = if name == "__name__" { "metric" } else { &name };
        held.insert(field.to_string(), Value::from(value.as_str()));
    }
    held.insert(
        "value".to_string(),
        Value::Number(tessaridb::Number::float(sample.value)),
    );
    let seconds = sample.at.div_euclid(1_000);
    let nanos = u32::try_from(sample.at.rem_euclid(1_000))
        .unwrap_or(0)
        .saturating_mul(1_000_000);
    held.insert(
        time.to_string(),
        Value::Datetime(
            tessaridb::Datetime::new(seconds, nanos)
                .unwrap_or_else(|| tessaridb::Datetime::from_seconds(seconds)),
        ),
    );
    Value::Object(held)
}

/// The `WriteRequest`, walked by hand: field 1 of the request is a
/// `TimeSeries`; field 1 of a series is a `Label`, field 2 a `Sample`; a
/// label is two strings; a sample a fixed64 double and a varint timestamp.
fn write_request(bytes: &[u8]) -> Result<Vec<Sample>, String> {
    let mut samples = Vec::new();
    let mut reader = Walker::new(bytes);
    while let Some((field, wire)) = reader.tag()? {
        if field == 1 && wire == 2 {
            let series = reader.delimited()?;
            samples.extend(time_series(series)?);
        } else {
            reader.skip(wire)?;
        }
    }
    Ok(samples)
}

/// One `TimeSeries` as the samples it holds, each carrying every label.
fn time_series(bytes: &[u8]) -> Result<Vec<Sample>, String> {
    let mut labels = Vec::new();
    let mut points = Vec::new();
    let mut reader = Walker::new(bytes);
    while let Some((field, wire)) = reader.tag()? {
        match (field, wire) {
            (1, 2) => labels.push(label(reader.delimited()?)?),
            (2, 2) => points.push(sample(reader.delimited()?)?),
            _ => reader.skip(wire)?,
        }
    }
    Ok(points
        .into_iter()
        .map(|(value, at)| Sample {
            labels: labels.clone(),
            value,
            at,
        })
        .collect())
}

/// One `Label`: two length-delimited strings.
fn label(bytes: &[u8]) -> Result<(String, String), String> {
    let mut name = String::new();
    let mut value = String::new();
    let mut reader = Walker::new(bytes);
    while let Some((field, wire)) = reader.tag()? {
        match (field, wire) {
            (1, 2) => name = reader.text()?,
            (2, 2) => value = reader.text()?,
            _ => reader.skip(wire)?,
        }
    }
    Ok((name, value))
}

/// One `Sample`: a fixed64 double and a varint timestamp.
fn sample(bytes: &[u8]) -> Result<(f64, i64), String> {
    let mut value = 0.0;
    let mut at = 0_i64;
    let mut reader = Walker::new(bytes);
    while let Some((field, wire)) = reader.tag()? {
        match (field, wire) {
            (1, 1) => value = f64::from_bits(reader.fixed64()?),
            (2, 0) => at = i64::try_from(reader.varint()?).unwrap_or(i64::MAX),
            _ => reader.skip(wire)?,
        }
    }
    Ok((value, at))
}

/// A protobuf walker: tags, varints, fixed64s and length-delimited runs, and
/// a refusal naming anything else.
pub(crate) struct Walker<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Walker<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// The next field's number and wire type, or `None` at the end.
    pub(crate) fn tag(&mut self) -> Result<Option<(u64, u64)>, String> {
        if self.at >= self.bytes.len() {
            return Ok(None);
        }
        let held = self.varint()?;
        let wire = held & 7;
        if wire != 0 && wire != 1 && wire != 2 {
            return Err(format!("a wire type this walker does not read: {wire}"));
        }
        Ok(Some((held >> 3, wire)))
    }

    pub(crate) fn varint(&mut self) -> Result<u64, String> {
        let mut held = 0_u64;
        let mut shift = 0_u32;
        loop {
            let byte = *self
                .bytes
                .get(self.at)
                .ok_or_else(|| "a varint that runs off the body".to_string())?;
            self.at = self.at.saturating_add(1);
            held |= u64::from(byte & 0x7f).checked_shl(shift).unwrap_or(0);
            if byte & 0x80 == 0 {
                return Ok(held);
            }
            shift = shift.saturating_add(7);
            if shift > 63 {
                return Err("a varint wider than 64 bits".to_string());
            }
        }
    }

    fn fixed64(&mut self) -> Result<u64, String> {
        let end = self.at.saturating_add(8);
        let raw: [u8; 8] = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| "a fixed64 that runs off the body".to_string())?
            .try_into()
            .map_err(|_| "a fixed64 that is not eight bytes".to_string())?;
        self.at = end;
        Ok(u64::from_le_bytes(raw))
    }

    pub(crate) fn delimited(&mut self) -> Result<&'a [u8], String> {
        let len = usize::try_from(self.varint()?)
            .map_err(|_| "a length this machine cannot hold".to_string())?;
        let end = self.at.saturating_add(len);
        let held = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| "a field that runs off the body".to_string())?;
        self.at = end;
        Ok(held)
    }

    pub(crate) fn text(&mut self) -> Result<String, String> {
        String::from_utf8(self.delimited()?.to_vec())
            .map_err(|_| "a label that is not UTF-8".to_string())
    }

    pub(crate) fn skip(&mut self, wire: u64) -> Result<(), String> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => {
                self.fixed64()?;
            }
            2 => {
                self.delimited()?;
            }
            _ => return Err(format!("a wire type this walker does not read: {wire}")),
        }
        Ok(())
    }
}
