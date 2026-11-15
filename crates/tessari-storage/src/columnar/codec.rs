//! The per-chunk encoding cascade.
//!
//! A chunk's data section holds the non-null values of one column in row
//! order, under one of five encodings chosen per chunk: dictionary, run
//! length, frame of reference plus bit packing, delta plus bit packing, or
//! plain. The worksheet (§4.3) sets the discipline: the encoding is recorded
//! in the chunk header — the reader never guesses — and the winner is chosen
//! on ratio AND decode throughput together, measured on the chunk's own rows
//! (the chunk is the sample unit).
//!
//! # The selection rule (D-SG2-4's initial thresholds, recorded here)
//!
//! Every candidate whose applicability test passes on the full value run is
//! encoded and then decoded once, and its decode time is measured. A
//! candidate decoding more than twice as slowly as the fastest candidate is
//! dropped — a decode-throughput guard — and the smallest data section wins
//! among those that remain. Ties keep the cascade order in [`Encoding::CASCADE`].
//!
//! # What each encoding assumes
//!
//! - **Plain** assumes nothing: length-prefixed payload-encoded values. It is
//!   the floor every other candidate must beat.
//! - **Dictionary** assumes repetition: distinct values in first-seen order,
//!   rows as codes. Low-cardinality columns earn their ratio here; a column
//!   of all-distinct values loses to plain on the code width alone.
//! - **Run length** assumes clustering: runs of one value. A sorted or
//!   append-ordered column (a sensor, a sequence) collapses to near-nothing.
//! - **Frame of reference** assumes numeric clustering: all values integers
//!   (or float bits, per the worksheet's v1 rule) inside a narrow band above
//!   a base. Decodes branch-free.
//! - **Delta** assumes a monotone integer run: the first value and
//!   non-negative deltas. Applicability is exact — it is checked on every
//!   value, not sampled, because a negative delta cannot be represented.
//!
//! # Numeric discipline
//!
//! Integers encode through `i128` intermediates and `TryFrom` conversions
//! everywhere: deltas are computed in the wider type so the full `i64` range
//! round-trips, and reconstruction that would leave it is a corruption
//! refusal, never a wrap. Base values cross the section as raw `u64` bits
//! (bit-preserving `to_ne_bytes`/`from_ne_bytes` helpers, not sign casts), so
//! the most negative integer keeps its identity. Floats encode as raw bits —
//! the worksheet's v1 rule — so every bit pattern, NaN and the infinities
//! included, round-trips exactly.

use std::collections::HashMap;
use std::time::Instant;

use tessari_types::{Number, Value};

use crate::error::{Error, Result};

/// Which encoding a chunk's data section carries.
///
/// The tag is written into the chunk header and is part of the format: tags
/// are never reused or renumbered, for the same reason the value tags are
/// not — data already written carries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// Length-prefixed values, in row order. The floor.
    Plain = 0,
    /// Distinct values in first-seen order; rows as codes into them.
    Dictionary = 1,
    /// Runs of one value as (code, length) pairs.
    RunLength = 2,
    /// Numeric band above a base, bit-packed deltas.
    FrameOfReference = 3,
    /// Monotone integers: first value, bit-packed non-negative deltas.
    Delta = 4,
}

impl Encoding {
    /// The tag the chunk header carries.
    pub(crate) const fn tag(self) -> u8 {
        match self {
            Self::Plain => 0,
            Self::Dictionary => 1,
            Self::RunLength => 2,
            Self::FrameOfReference => 3,
            Self::Delta => 4,
        }
    }

    /// The encoding a header tag names.
    fn from_tag(found: u8) -> Result<Self> {
        match found {
            0 => Ok(Self::Plain),
            1 => Ok(Self::Dictionary),
            2 => Ok(Self::RunLength),
            3 => Ok(Self::FrameOfReference),
            4 => Ok(Self::Delta),
            _ => Err(Error::ColumnarUnknownEncoding { found }),
        }
    }

    /// Cascade order: the order ties keep.
    const CASCADE: [Self; 5] = [
        Self::Dictionary,
        Self::RunLength,
        Self::FrameOfReference,
        Self::Delta,
        Self::Plain,
    ];
}

/// A candidate that survived applicability, with its encoded form and the
/// measured cost of decoding it.
struct Candidate {
    encoding: Encoding,
    bytes: Vec<u8>,
    decoded_nanos: u128,
}

/// Choose the encoding for a value run and encode it.
///
/// `values` holds the non-null values of one column in row order. Infallible
/// by construction: the writer is trusted, and every candidate's
/// applicability is checked before it runs, so plain — which assumes nothing
/// — always survives.
#[must_use]
pub(crate) fn choose(values: &[Value]) -> (Encoding, Vec<u8>) {
    let mut candidates = Vec::with_capacity(Encoding::CASCADE.len());
    for encoding in Encoding::CASCADE {
        let Some(bytes) = encode(encoding, values) else {
            continue;
        };
        // The candidate must decode to be worth its ratio; a decode failure
        // on bytes this process just wrote is a bug, and carrying a broken
        // candidate into the selection would paper over it.
        let started = Instant::now();
        let mut check = Cursor::new(&bytes);
        let decoded = decode(encoding.tag(), &mut check, values.len());
        let decoded_nanos = started.elapsed().as_nanos();
        match decoded {
            Ok(got) if got == values && check.finish().is_ok() => {}
            _ => {
                debug_assert!(false, "{encoding:?} did not round-trip its own bytes");
                continue;
            }
        }
        candidates.push(Candidate {
            encoding,
            bytes,
            decoded_nanos,
        });
    }

    // Throughput guard: anything decoding more than twice as slowly as the
    // fastest candidate is dropped before the ratio comparison. The 2× is
    // D-SG2-4's initial threshold, recorded in the module docs.
    if let Some(fastest) = candidates.iter().map(|c| c.decoded_nanos).min() {
        let ceiling = fastest.saturating_mul(2);
        candidates.retain(|c| c.decoded_nanos <= ceiling);
    }

    // Smallest data section wins; stable sort keeps cascade order on ties.
    candidates.sort_by_key(|c| c.bytes.len());
    candidates
        .into_iter()
        .next()
        .map_or((Encoding::Plain, Vec::new()), |winner| {
            (winner.encoding, winner.bytes)
        })
}

/// Encode a value run under one encoding.
///
/// `None` when the encoding does not apply to this run (checked on every
/// value, not sampled — a run that breaks an assumption must fall back, not
/// truncate).
pub(crate) fn encode(encoding: Encoding, values: &[Value]) -> Option<Vec<u8>> {
    match encoding {
        Encoding::Plain => Some(encode_plain(values)),
        Encoding::Dictionary => Some(encode_dictionary(values)),
        Encoding::RunLength => Some(encode_runs(values)),
        Encoding::FrameOfReference => encode_reference(values),
        Encoding::Delta => encode_delta(values),
    }
}

/// Decode a data section back into a value run.
///
/// `count` is the number of non-null values the section must hold, `cursor`
/// is positioned at the section's first byte and left exactly past its last —
/// the caller owns the wider layout's boundary and calls `Cursor::finish`
/// when the section is the whole input. Hostile input is refused with
/// [`Error::ColumnarTruncated`] or [`Error::ColumnarUnknownEncoding`] — never
/// guessed at, never a panic.
pub(crate) fn decode(
    encoding_tag: u8,
    cursor: &mut Cursor<'_>,
    count: usize,
) -> Result<Vec<Value>> {
    let encoding = Encoding::from_tag(encoding_tag)?;
    match encoding {
        Encoding::Plain => decode_plain(cursor, count),
        Encoding::Dictionary => decode_dictionary(cursor, count),
        Encoding::RunLength => decode_runs(cursor, count),
        Encoding::FrameOfReference => decode_reference(cursor, count),
        Encoding::Delta => decode_delta(cursor, count),
    }
}

/// The bit-preserving `i64` ↔ `u64` crossings a section base travels.
///
/// Not sign casts: the base may be `i64::MIN`, which a sign cast would turn
/// into a positive delta and silently corrupt the band.
fn bits_of(value: i64) -> u64 {
    u64::from_ne_bytes(value.to_ne_bytes())
}

fn i64_of(bits: u64) -> i64 {
    i64::from_ne_bytes(bits.to_ne_bytes())
}

/// Plain: the count, then each value length-prefixed.
fn encode_plain(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    put_u32(&mut out, values.len());
    for value in values {
        put_value(&mut out, value);
    }
    out
}

fn decode_plain(cursor: &mut Cursor<'_>, count: usize) -> Result<Vec<Value>> {
    let held = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
    if held != count {
        return Err(cursor.truncated());
    }
    let mut values = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        values.push(cursor.value()?);
    }
    Ok(values)
}

/// Dictionary: distinct values in first-seen order, rows as `u32` codes.
fn encode_dictionary(values: &[Value]) -> Vec<u8> {
    let mut distinct: Vec<Value> = Vec::new();
    let mut positions: HashMap<Value, usize> = HashMap::new();
    let mut codes = Vec::with_capacity(values.len());
    for value in values {
        let code = match positions.get(value) {
            Some(code) => *code,
            None => {
                let code = distinct.len();
                distinct.push(value.clone());
                positions.insert(value.clone(), code);
                code
            }
        };
        codes.push(code);
    }
    let mut out = Vec::new();
    put_u32(&mut out, distinct.len());
    for value in &distinct {
        put_value(&mut out, value);
    }
    put_u32(&mut out, codes.len());
    for code in codes {
        put_u32(&mut out, code);
    }
    out
}

fn decode_dictionary(cursor: &mut Cursor<'_>, count: usize) -> Result<Vec<Value>> {
    let distinct_count = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
    let mut distinct = Vec::with_capacity(distinct_count.min(1024));
    for _ in 0..distinct_count {
        distinct.push(cursor.value()?);
    }
    let held = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
    if held != count {
        return Err(cursor.truncated());
    }
    let mut values = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let code = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
        let value = distinct.get(code).ok_or_else(|| cursor.truncated())?;
        values.push(value.clone());
    }
    Ok(values)
}

/// Run length: the distinct values, then (code, run length) pairs.
fn encode_runs(values: &[Value]) -> Vec<u8> {
    let mut distinct: Vec<Value> = Vec::new();
    let mut positions: HashMap<Value, usize> = HashMap::new();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for value in values {
        let code = match positions.get(value) {
            Some(code) => *code,
            None => {
                let code = distinct.len();
                distinct.push(value.clone());
                positions.insert(value.clone(), code);
                code
            }
        };
        match runs.last_mut() {
            Some((last_code, run)) if *last_code == code => {
                *run = run.saturating_add(1);
            }
            _ => runs.push((code, 1)),
        }
    }
    let mut out = Vec::new();
    put_u32(&mut out, distinct.len());
    for value in &distinct {
        put_value(&mut out, value);
    }
    put_u32(&mut out, runs.len());
    for (code, run) in runs {
        put_u32(&mut out, code);
        put_u32(&mut out, run);
    }
    out
}

fn decode_runs(cursor: &mut Cursor<'_>, count: usize) -> Result<Vec<Value>> {
    let distinct_count = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
    let mut distinct = Vec::with_capacity(distinct_count.min(1024));
    for _ in 0..distinct_count {
        distinct.push(cursor.value()?);
    }
    let run_count = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
    let mut values = Vec::with_capacity(count.min(1024));
    for _ in 0..run_count {
        let code = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
        let run = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
        let value = distinct.get(code).ok_or_else(|| cursor.truncated())?;
        if values.len().saturating_add(run) > count {
            return Err(cursor.truncated());
        }
        values.extend(std::iter::repeat_with(|| value.clone()).take(run));
    }
    if values.len() != count {
        return Err(cursor.truncated());
    }
    Ok(values)
}

/// Frame of reference: a numeric base and bit-packed deltas above it.
///
/// Applies when every value is an integer (base = minimum) or every value is
/// a float held as raw bits (base = minimum bit pattern). The worksheet's v1
/// rule: floats encode on their raw bits; wider numeric work (ALP) is a
/// recorded later candidate, not a promise.
fn encode_reference(values: &[Value]) -> Option<Vec<u8>> {
    if let Some(integers) = integer_run(values) {
        let base = *integers.iter().min()?;
        let mut deltas = Vec::with_capacity(integers.len());
        let mut widest = 0_u64;
        for value in &integers {
            // i128 intermediates keep the full i64 range exact.
            let delta = i128::from(*value).checked_sub(i128::from(base))?;
            let delta = u64::try_from(delta).ok()?;
            widest = widest.max(delta);
            deltas.push(delta);
        }
        let width = bit_width(widest);
        let mut out = Vec::new();
        out.push(1); // integer band
        out.push(width);
        put_u64(&mut out, bits_of(base));
        out.extend(pack(width, &deltas));
        Some(out)
    } else {
        let bits = float_run(values)?;
        let base = *bits.iter().min()?;
        let mut deltas = Vec::with_capacity(bits.len());
        let mut widest = 0_u64;
        for value in &bits {
            let delta = value.wrapping_sub(base);
            widest = widest.max(delta);
            deltas.push(delta);
        }
        let width = bit_width(widest);
        let mut out = Vec::new();
        out.push(2); // float band over raw bits
        out.push(width);
        put_u64(&mut out, base);
        out.extend(pack(width, &deltas));
        Some(out)
    }
}

fn decode_reference(cursor: &mut Cursor<'_>, count: usize) -> Result<Vec<Value>> {
    let band = cursor.u8()?;
    let width = cursor.u8()?;
    let base = cursor.u64()?;
    let deltas = unpack(width, count, cursor)?;
    match band {
        1 => {
            let base = i128::from(i64_of(base));
            let mut values = Vec::with_capacity(count.min(1024));
            for delta in deltas {
                let value = base
                    .checked_add(i128::from(delta))
                    .ok_or_else(|| cursor.truncated())?;
                let value = i64::try_from(value).map_err(|_| cursor.truncated())?;
                values.push(Value::Number(Number::Integer(value)));
            }
            Ok(values)
        }
        2 => {
            let mut values = Vec::with_capacity(count.min(1024));
            for delta in deltas {
                let bits = base.wrapping_add(delta);
                values.push(Value::Number(Number::Float(f64::from_bits(bits))));
            }
            Ok(values)
        }
        _ => Err(cursor.truncated()),
    }
}

/// Delta: the first value and bit-packed non-negative deltas, for a monotone
/// integer run. Applicability is exact — it is checked on every value.
fn encode_delta(values: &[Value]) -> Option<Vec<u8>> {
    let integers = integer_run(values)?;
    let first = *integers.first()?;
    let mut previous = first;
    let mut deltas = Vec::with_capacity(integers.len().saturating_sub(1));
    let mut widest = 0_u64;
    for value in integers.iter().skip(1) {
        let delta = i128::from(*value).checked_sub(i128::from(previous))?;
        let delta = u64::try_from(delta).ok()?;
        widest = widest.max(delta);
        deltas.push(delta);
        previous = *value;
    }
    let width = bit_width(widest);
    let mut out = Vec::new();
    out.push(width);
    put_u64(&mut out, bits_of(first));
    out.extend(pack(width, &deltas));
    Some(out)
}

fn decode_delta(cursor: &mut Cursor<'_>, count: usize) -> Result<Vec<Value>> {
    let width = cursor.u8()?;
    let first = i64_of(cursor.u64()?);
    let mut previous = first;
    let mut values = Vec::with_capacity(count.min(1024));
    values.push(Value::Number(Number::Integer(previous)));
    for delta in unpack(width, count.saturating_sub(1), cursor)? {
        let value = i128::from(previous)
            .checked_add(i128::from(delta))
            .ok_or_else(|| cursor.truncated())?;
        previous = i64::try_from(value).map_err(|_| cursor.truncated())?;
        values.push(Value::Number(Number::Integer(previous)));
    }
    Ok(values)
}

/// Every value an integer, as its `i64`.
fn integer_run(values: &[Value]) -> Option<Vec<i64>> {
    values
        .iter()
        .map(|value| match value {
            Value::Number(Number::Integer(integer)) => Some(*integer),
            _ => None,
        })
        .collect()
}

/// Every value a float, as its raw bits.
fn float_run(values: &[Value]) -> Option<Vec<u64>> {
    values
        .iter()
        .map(|value| match value {
            Value::Number(Number::Float(float)) => Some(float.to_bits()),
            _ => None,
        })
        .collect()
}

/// Bits needed to hold `widest`, 0..=64.
fn bit_width(widest: u64) -> u8 {
    u8::try_from(
        u64::BITS
            .checked_sub(widest.leading_zeros())
            .unwrap_or(u32::MAX),
    )
    .unwrap_or(u8::MAX)
}

/// Pack fixed-width values LSB-first.
fn pack(width: u8, values: &[u64]) -> Vec<u8> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(
        values
            .len()
            .saturating_mul(usize::from(width))
            .saturating_div(8)
            .saturating_add(1),
    );
    let mut acc: u128 = 0;
    let mut held: u32 = 0;
    for &value in values {
        acc |= u128::from(value) << held;
        held = held.saturating_add(u32::from(width));
        while held >= 8 {
            out.push(u8::try_from(acc & 0xff).unwrap_or(0));
            acc >>= 8;
            held = held.saturating_sub(8);
        }
    }
    if held > 0 {
        out.push(u8::try_from(acc & 0xff).unwrap_or(0));
    }
    out
}

/// Unpack `count` fixed-width values from LSB-first bytes.
fn unpack(width: u8, count: usize, cursor: &mut Cursor<'_>) -> Result<Vec<u64>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if width == 0 {
        return Ok(vec![0; count]);
    }
    let mask = if width >= 64 {
        u64::MAX
    } else {
        (1_u64 << u32::from(width)).wrapping_sub(1)
    };
    let mut values = Vec::with_capacity(count.min(1024));
    let mut acc: u128 = 0;
    let mut held: u32 = 0;
    for _ in 0..count {
        while held < u32::from(width) {
            acc |= u128::from(cursor.u8()?) << held;
            held = held.saturating_add(8);
        }
        values.push(u64::try_from(acc & u128::from(mask)).unwrap_or(u64::MAX));
        acc >>= u32::from(width);
        held = held.saturating_sub(u32::from(width));
    }
    Ok(values)
}

fn put_u32(out: &mut Vec<u8>, value: usize) {
    // A value run never exceeds a sealed chunk's row cap (columnar::chunk
    // enforces it), so the conversion cannot fail; the saturation idiom is
    // the codebase's own (tessari-encoding scalars), keeping the bytes
    // writable rather than panicking on a bug.
    out.extend_from_slice(&u32::try_from(value).unwrap_or(u32::MAX).to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// One payload-encoded value, length-prefixed.
fn put_value(out: &mut Vec<u8>, value: &Value) {
    let encoded = tessari_encoding::encode_payload(value);
    put_u32(out, encoded.len());
    out.extend_from_slice(&encoded.into_bytes());
}

/// A bounds-checked cursor over a section's bytes.
///
/// The reader side of the codec faces bytes the store hands it, which are
/// only as trustworthy as whatever wrote them; every take is checked, and an
/// overrun is [`Error::ColumnarTruncated`], never an index panic.
pub(crate) struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    /// A cursor over `bytes`, positioned at the start.
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    /// The overrun error naming what was needed against what is held.
    pub(crate) fn truncated(&self) -> Error {
        Error::ColumnarTruncated {
            needed: self.position.saturating_add(1),
            len: self.bytes.len(),
        }
    }

    /// Take `len` bytes, advancing.
    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(len)
            .ok_or_else(|| self.truncated())?;
        let slice = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| self.truncated())?;
        self.position = end;
        Ok(slice)
    }

    /// One byte.
    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// A big-endian `u32`.
    pub(crate) fn u32(&mut self) -> Result<u32> {
        let raw: [u8; 4] = self.take(4)?.try_into().map_err(|_| self.truncated())?;
        Ok(u32::from_be_bytes(raw))
    }

    /// A big-endian `u64`.
    pub(crate) fn u64(&mut self) -> Result<u64> {
        let raw: [u8; 8] = self.take(8)?.try_into().map_err(|_| self.truncated())?;
        Ok(u64::from_be_bytes(raw))
    }

    /// One length-prefixed payload-encoded value.
    pub(crate) fn value(&mut self) -> Result<Value> {
        let len = usize::try_from(self.u32()?).unwrap_or(usize::MAX);
        let bytes = self.take(len)?;
        tessari_encoding::decode_payload(bytes).map_err(Error::from)
    }

    /// Refuse trailing bytes: a section that does not consume its input
    /// exactly is a section that is not the shape it claims.
    pub(crate) fn finish(&self) -> Result<()> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(self.truncated())
        }
    }
}
