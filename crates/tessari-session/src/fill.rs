//! `FILL <mode> FROM <start> TO <end>`: one row per window of a stated range,
//! including the windows nothing was written in (ADR-0088 §2).
//!
//! # Why the range is part of the clause
//!
//! A window with no records has no row, and the reason was never that filling
//! is hard: it is that a grouping does not say which windows it meant. The data
//! cannot answer that — its extent is exactly what a gap removes from the edges.
//! So the range is written, and the rows outside it are not answered.
//!
//! # Why a filled row can always be told from a written one
//!
//! A `count` in a filled window answers `0` whatever the mode, because a count
//! of nothing is exact rather than invented; every other fold takes the mode.
//! The answer also carries a note saying how many windows were filled.

use std::collections::BTreeMap;

use tessari_ql::{Expr, ExprKind, Fill, FillMode, Function, Projected, Span};
use tessari_storage::Transaction;
use tessari_types::{Datetime, Number, RecordId, Value};

use crate::aggregate::approximate;
use crate::error::{Error, Result};
use crate::evaluate::Scope;
use crate::session::Session;

/// The most windows one read may fill, across every group: a million rows is a
/// read somebody meant to page, not a range somebody meant to write.
const MOST_WINDOWS: u64 = 1_000_000;

/// A grouped row before it is answered: its group key, the identity it answers
/// under, and its projected fields.
pub(crate) type Row = (Vec<Value>, RecordId, BTreeMap<String, Value>);

/// One group's written windows, by the second each starts at.
type Written = BTreeMap<i64, (RecordId, BTreeMap<String, Value>)>;

/// The written window nearest one side of a filled one.
type Neighbour<'a> = Option<(&'a i64, &'a (RecordId, BTreeMap<String, Value>))>;

/// The windowed key a fill completes: where it sits in `GROUP BY`, and its width
/// in seconds.
struct Window {
    position: usize,
    seconds: i64,
}

impl Session<'_> {
    /// Complete a windowed grouping over the stated range, answering the rows in
    /// the grouping's own order and how many of them were filled.
    ///
    /// # Errors
    ///
    /// [`Error::FillNeedsWindow`] unless exactly one key is a `time::bucket` of a
    /// constant width, [`Error::FillNeedsRange`] unless both ends are instants,
    /// [`Error::FillTooWide`] past [`MOST_WINDOWS`], and whatever evaluating a
    /// fill value refuses.
    pub(crate) fn fill_windows(
        &self,
        transaction: &mut Transaction<'_>,
        rows: Vec<Row>,
        wanted: &[Projected],
        group: &[Expr],
        fill: &Fill,
    ) -> Result<(Vec<(RecordId, Value)>, u64)> {
        let window = self.window_of(transaction, group, fill.span)?;
        let (first, end) = self.range_of(transaction, fill, window.seconds)?;

        // Written rows by the rest of their key, then by window start.
        let mut by_rest: BTreeMap<Vec<Value>, Written> = BTreeMap::new();
        if group.len() == 1 {
            by_rest.insert(Vec::new(), BTreeMap::new());
        }
        for (key, id, fields) in rows {
            let Some(Value::Datetime(at)) = key.get(window.position) else {
                continue;
            };
            let mut rest = key.clone();
            rest.remove(window.position);
            by_rest
                .entry(rest)
                .or_default()
                .insert(at.seconds(), (id, fields));
        }

        let per_group = if end > first {
            let span = end.checked_sub(first).unwrap_or(0);
            let count = span
                .checked_add(window.seconds.checked_sub(1).unwrap_or(0))
                .and_then(|widened| widened.checked_div(window.seconds))
                .unwrap_or(0);
            u64::try_from(count).unwrap_or(u64::MAX)
        } else {
            0
        };
        let total = per_group.saturating_mul(u64::try_from(by_rest.len()).unwrap_or(u64::MAX));
        if total > MOST_WINDOWS {
            return Err(Error::FillTooWide {
                windows: total,
                most: MOST_WINDOWS,
                span: fill.span,
            });
        }
        let constant = match &fill.mode {
            FillMode::Value(expr) => Some(self.evaluate_in(transaction, expr, Scope::none())?),
            FillMode::Previous | FillMode::Linear => None,
        };

        let mut answered: BTreeMap<Vec<Value>, (RecordId, Value)> = BTreeMap::new();
        let mut filled = 0_u64;
        for (rest, written) in &by_rest {
            let mut start = first;
            while start < end {
                let mut key = rest.clone();
                let instant = Datetime::from_seconds(start);
                key.insert(window.position, Value::Datetime(instant));
                let row = if let Some((id, fields)) = written.get(&start) {
                    (id.clone(), Value::Object(fields.clone()))
                } else {
                    filled = filled.saturating_add(1);
                    let fields = filled_fields(
                        wanted,
                        group,
                        &key,
                        written,
                        start,
                        &fill.mode,
                        constant.as_ref(),
                    );
                    (window_identity(start), Value::Object(fields))
                };
                answered.insert(key, row);
                let Some(next) = start.checked_add(window.seconds) else {
                    break;
                };
                start = next;
            }
        }
        Ok((answered.into_values().collect(), filled))
    }

    /// The one `time::bucket` key and its width.
    fn window_of(
        &self,
        transaction: &mut Transaction<'_>,
        group: &[Expr],
        span: Span,
    ) -> Result<Window> {
        let mut found = None;
        for (position, key) in group.iter().enumerate() {
            let ExprKind::Call {
                function: Function::TimeBucket,
                arguments,
                ..
            } = &key.kind
            else {
                continue;
            };
            if found.is_some() {
                return Err(Error::FillNeedsWindow { span });
            }
            let width = match arguments.get(1) {
                Some(width) => self.evaluate_in(transaction, width, Scope::none())?,
                None => Value::None,
            };
            let Value::Duration(width) = width else {
                return Err(Error::FillNeedsWindow { span });
            };
            if width.seconds() <= 0 || width.nanos() != 0 {
                return Err(Error::FillNeedsWindow { span });
            }
            found = Some(Window {
                position,
                seconds: width.seconds(),
            });
        }
        found.ok_or(Error::FillNeedsWindow { span })
    }

    /// The first window's start and the range's end, in seconds.
    fn range_of(
        &self,
        transaction: &mut Transaction<'_>,
        fill: &Fill,
        width: i64,
    ) -> Result<(i64, i64)> {
        let from = self.evaluate_in(transaction, &fill.from, Scope::none())?;
        let to = self.evaluate_in(transaction, &fill.to, Scope::none())?;
        let (Value::Datetime(from), Value::Datetime(to)) = (from, to) else {
            return Err(Error::FillNeedsRange { span: fill.span });
        };
        let first = from.seconds().div_euclid(width).saturating_mul(width);
        // A range ending inside a second still covers the window that second
        // belongs to, because `TO` is exclusive of the instant, not of the second.
        let end = if to.nanos() > 0 {
            to.seconds().saturating_add(1)
        } else {
            to.seconds()
        };
        Ok((first, end))
    }
}

/// The fields of a window nothing was written in.
fn filled_fields(
    wanted: &[Projected],
    group: &[Expr],
    key: &[Value],
    written: &Written,
    start: i64,
    mode: &FillMode,
    constant: Option<&Value>,
) -> BTreeMap<String, Value> {
    let before = written.range(..start).next_back();
    let after = written.range(start..).next();
    let mut fields = BTreeMap::new();
    for value in wanted {
        let name = &value.name.text;
        let answer = if let Some(at) = group.iter().position(|held| held.same_shape(&value.value)) {
            key.get(at).cloned().unwrap_or(Value::None)
        } else if matches!(
            value.value.kind,
            ExprKind::Fold {
                fold: tessari_ql::Aggregate::Count,
                ..
            }
        ) {
            Value::Number(Number::Integer(0))
        } else {
            match mode {
                FillMode::Value(_) => constant.cloned().unwrap_or(Value::Null),
                FillMode::Previous => before
                    .and_then(|(_, (_, fields))| fields.get(name).cloned())
                    .unwrap_or(Value::Null),
                FillMode::Linear => interpolated(name, before, after, start),
            }
        };
        if answer.is_present() {
            fields.insert(name.clone(), answer);
        }
    }
    fields
}

/// The straight line between the written windows either side, at `start`; `NULL`
/// at an edge or between values that are not both numbers.
fn interpolated(name: &str, before: Neighbour<'_>, after: Neighbour<'_>, start: i64) -> Value {
    let (Some((t0, (_, left))), Some((t1, (_, right)))) = (before, after) else {
        return Value::Null;
    };
    let (Some(Value::Number(v0)), Some(Value::Number(v1))) = (left.get(name), right.get(name))
    else {
        return Value::Null;
    };
    let (Some(v0), Some(v1)) = (approximate(v0), approximate(v1)) else {
        return Value::Null;
    };
    let seconds = |from: i64, to: i64| {
        to.checked_sub(from)
            .and_then(|elapsed| approximate(&Number::Integer(elapsed)))
    };
    let (Some(along), Some(across)) = (seconds(*t0, start), seconds(*t0, *t1)) else {
        return Value::Null;
    };
    Value::Number(Number::Float(v0 + (v1 - v0) * along / across))
}

/// The identity a filled row answers under: the smallest UUID version 7 of its
/// window's first millisecond, so filled rows sort among written ones by time.
fn window_identity(start: i64) -> RecordId {
    let millis = u64::try_from(start)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000))
        .unwrap_or(0);
    let mut bytes = [0_u8; 16];
    let [_, _, t0, t1, t2, t3, t4, t5] = millis.to_be_bytes();
    bytes[..6].copy_from_slice(&[t0, t1, t2, t3, t4, t5]);
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    RecordId::Uuid(bytes)
}
