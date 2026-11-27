//! The product-analytics folds — `window_funnel`, `sequence_match`,
//! `sequence_count` and `retention` — over a group's events.
//!
//! An event is the record's instant and which of the statement's conditions
//! it met, so the whole of what a group contributes is a list of
//! `(instant, [bool, …])`. The folds hold exactly that, sort it by instant
//! once at the finish, and answer from the order — which is why they are
//! whole-group folds, by `median`'s rule: no running form knows the longest
//! chain before the last event has arrived.
//!
//! The sequence folds read a small pattern language, `'(1)(<1800)(2)'`:
//! `(n)` is the n-th condition, and `(<s)` / `(>s)` between two of them bound
//! the seconds between the events that meet them. A chain with time bounds is
//! not writable as nested calls, which is the whole reason the pattern earns
//! its place; anything outside the three token shapes is refused rather than
//! guessed at.

use tessari_ql::{Aggregate, Span};
use tessari_types::{Datetime, Number, Value};

use super::failed;
use crate::error::Result;

/// One event of a group: when it happened, and which conditions it met.
pub(crate) type Event = (Datetime, Vec<bool>);

/// What one record offered: its event, and the fold's settled second
/// argument when the record carried one.
pub(crate) type Offered = (Datetime, Vec<bool>, Option<Value>);

/// One step of a sequence pattern: which condition, and the bounds on the
/// seconds since the step before it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Step {
    /// The condition's place, counted from zero.
    pub(crate) cond: usize,
    /// `(>s)` — at least this many seconds after the step before.
    pub(crate) after: Option<f64>,
    /// `(<s)` — less than this many seconds after the step before.
    pub(crate) within: Option<f64>,
}

/// The pattern, read once when a record first offers it.
///
/// # Errors
///
/// A pattern that is not a run of `(n)`, `(<s)` and `(>s)` tokens, or that
/// names no condition at all, is refused naming the fold.
pub(crate) fn pattern_of(text: &str, span: Span) -> Result<Vec<Step>> {
    let refuse = |reason| failed("sequence_match", reason, span);
    let mut steps: Vec<Step> = Vec::new();
    let mut pending: (Option<f64>, Option<f64>) = (None, None);
    let mut rest = text.trim();
    while !rest.is_empty() {
        let Some(inner) = rest
            .strip_prefix('(')
            .and_then(|after| after.split_once(')'))
        else {
            return Err(refuse("a pattern of (n), (<s) and (>s) tokens"));
        };
        let (token, tail) = inner;
        let token = token.trim();
        if let Some(bound) = token.strip_prefix('<') {
            let Ok(seconds) = bound.trim().parse::<f64>() else {
                return Err(refuse("a bound that is not a number of seconds"));
            };
            if pending.1.is_some() {
                return Err(refuse("two upper bounds between one pair of steps"));
            }
            pending.1 = Some(seconds);
        } else if let Some(bound) = token.strip_prefix('>') {
            let Ok(seconds) = bound.trim().parse::<f64>() else {
                return Err(refuse("a bound that is not a number of seconds"));
            };
            if pending.0.is_some() {
                return Err(refuse("two lower bounds between one pair of steps"));
            }
            pending.0 = Some(seconds);
        } else {
            let Ok(place) = token.parse::<usize>() else {
                return Err(refuse("a condition written as its number, from 1"));
            };
            if place == 0 {
                return Err(refuse("a condition written as its number, from 1"));
            }
            steps.push(Step {
                cond: place.saturating_sub(1),
                after: pending.0,
                within: pending.1,
            });
            pending = (None, None);
        }
        rest = tail.trim();
    }
    if steps.is_empty() {
        return Err(refuse("a pattern naming at least one condition"));
    }
    if pending != (None, None) {
        return Err(refuse("a bound with no condition after it"));
    }
    Ok(steps)
}

/// The seconds between two instants, as a float.
fn gap(earlier: Datetime, later: Datetime) -> f64 {
    let whole = later.seconds().saturating_sub(earlier.seconds());
    let nanos = i64::from(later.nanos()).saturating_sub(i64::from(earlier.nanos()));
    crate::aggregate::approximate(&Number::Integer(whole)).unwrap_or(f64::NAN)
        + crate::aggregate::approximate(&Number::Integer(nanos)).unwrap_or(f64::NAN) / 1e9
}

/// Whether the event at `at` met the step's condition, within the step's
/// bounds since `last`.
fn admits(step: &Step, event: &Event, last: Datetime, first: bool) -> bool {
    if !event.1.get(step.cond).copied().unwrap_or(false) {
        return false;
    }
    if first {
        return true;
    }
    let since = gap(last, event.0);
    if let Some(after) = step.after
        && since <= after
    {
        return false;
    }
    if let Some(within) = step.within
        && since >= within
    {
        return false;
    }
    true
}

/// The longest chain from `start`, greedily taking the earliest event that
/// admits each next step; how many steps it reached and where it ended.
fn chain_from(events: &[Event], steps: &[Step], start: usize) -> (usize, usize) {
    let Some(first) = events.get(start) else {
        return (0, start);
    };
    if !first
        .1
        .get(steps.first().map(|step| step.cond).unwrap_or(0))
        .copied()
        .unwrap_or(false)
    {
        return (0, start);
    }
    let mut reached = 1_usize;
    let mut last = first.0;
    let mut end = start;
    for (place, event) in events.iter().enumerate().skip(start.saturating_add(1)) {
        let Some(step) = steps.get(reached) else {
            break;
        };
        if admits(step, event, last, false) {
            reached = reached.saturating_add(1);
            last = event.0;
            end = place;
            if reached == steps.len() {
                break;
            }
        }
    }
    (reached, end)
}

/// What `window_funnel` answers: the longest prefix of the conditions met by
/// a chain of events in time order, the chain inside `window` seconds of its
/// own first event.
pub(crate) fn funnel(events: &[Event], window: f64) -> Value {
    let mut ordered = events.to_vec();
    ordered.sort_by_key(|(at, _)| *at);
    let width = ordered.first().map_or(0, |(_, conds)| conds.len());
    let mut best = 0_usize;
    for (start, first) in ordered.iter().enumerate() {
        if !first.1.first().copied().unwrap_or(false) {
            continue;
        }
        let mut reached = 1_usize;
        for event in ordered.iter().skip(start.saturating_add(1)) {
            if gap(first.0, event.0) > window {
                break;
            }
            if event.1.get(reached).copied().unwrap_or(false) {
                reached = reached.saturating_add(1);
                if reached == width {
                    break;
                }
            }
        }
        best = best.max(reached);
    }
    Value::Number(Number::Integer(i64::try_from(best).unwrap_or(i64::MAX)))
}

/// What `sequence_match` answers: whether any chain meets the whole pattern.
pub(crate) fn matched(events: &[Event], steps: &[Step]) -> Value {
    let mut ordered = events.to_vec();
    ordered.sort_by_key(|(at, _)| *at);
    let found = ordered
        .iter()
        .enumerate()
        .any(|(start, _)| chain_from(&ordered, steps, start).0 == steps.len());
    Value::Number(Number::Integer(i64::from(found)))
}

/// What `sequence_count` answers: how many non-overlapping chains the group
/// holds, each found from the earliest start not yet used and the scan
/// resuming past the chain's last event.
pub(crate) fn counted(events: &[Event], steps: &[Step]) -> Value {
    let mut ordered = events.to_vec();
    ordered.sort_by_key(|(at, _)| *at);
    let mut chains = 0_u64;
    let mut from = 0_usize;
    while from < ordered.len() {
        let mut found = None;
        for start in from..ordered.len() {
            let (reached, end) = chain_from(&ordered, steps, start);
            if reached == steps.len() {
                found = Some(end);
                break;
            }
        }
        match found {
            Some(end) => {
                chains = chains.saturating_add(1);
                from = end.saturating_add(1);
            }
            None => break,
        }
    }
    Value::Number(Number::Integer(i64::try_from(chains).unwrap_or(i64::MAX)))
}

/// What `retention` answers: one `1` or `0` per condition — the anchor met
/// at all, then each later condition met within a group the anchor was met
/// in. Over no events, `NONE`, by `mean`'s rule: a cohort of nobody retains
/// nothing and zero would be a claim about somebody.
pub(crate) fn retained(events: &[Event]) -> Value {
    let Some((_, first)) = events.first() else {
        return Value::None;
    };
    let width = first.len();
    let anchor = events
        .iter()
        .any(|(_, conds)| conds.first().copied().unwrap_or(false));
    let mut answered = Vec::with_capacity(width);
    for place in 0..width {
        let met = anchor
            && events
                .iter()
                .any(|(_, conds)| conds.get(place).copied().unwrap_or(false));
        answered.push(Value::Number(Number::Integer(i64::from(met))));
    }
    Value::Array(answered)
}

/// The event a record offered, read by the fold that was offered it; `None`
/// for a record that holds no event (an absent instant or conditions).
///
/// # Errors
///
/// A value of the wrong shape is refused naming the fold, rather than read as
/// an event it is not.
pub(crate) fn event_of(fold: Aggregate, value: &Value, span: Span) -> Result<Option<Offered>> {
    // `retention`'s whole value is the conditions array; the sequence folds
    // are offered `[instant, extra, conditions]`.
    let (instant, extra, conds) = if fold == Aggregate::Retention {
        (None, None, Some(value.clone()))
    } else {
        let Value::Array(parts) = value else {
            return Ok(None);
        };
        (
            parts.first().cloned(),
            parts.get(1).cloned(),
            parts.get(2).cloned(),
        )
    };
    let conds = match conds {
        Some(Value::Array(conds)) => conds,
        // An absent condition list is a record with no event in it; anything
        // else in its place is a mistake in the statement.
        Some(Value::None) | Some(Value::Null) | None => return Ok(None),
        Some(_) => {
            return Err(failed(
                fold.spelling(),
                "conditions written as one array of booleans",
                span,
            ));
        }
    };
    let mut met = Vec::with_capacity(conds.len());
    for cond in conds {
        match cond {
            Value::Bool(held) => met.push(held),
            // A condition over a field the record does not have did not hold.
            Value::None | Value::Null => met.push(false),
            other => {
                return Err(failed(
                    fold.spelling(),
                    match other {
                        Value::Number(_) => "a condition that is a number, not a boolean",
                        _ => "a condition that is not a boolean",
                    },
                    span,
                ));
            }
        }
    }
    let Some(Value::Datetime(at)) = instant else {
        if fold == Aggregate::Retention {
            return Ok(Some((Datetime::from_seconds(0), met, None)));
        }
        return match instant {
            Some(Value::None) | Some(Value::Null) | None => Ok(None),
            Some(_) => Err(failed(
                fold.spelling(),
                "an instant that is not a datetime",
                span,
            )),
        };
    };
    Ok(Some((at, met, extra)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::{Event, counted, funnel, matched, pattern_of, retained};
    use tessari_ql::Span;
    use tessari_types::{Datetime, Value};

    /// An event at `second` meeting the listed conditions of three.
    fn event(second: i64, met: &[usize]) -> Event {
        let mut conds = vec![false; 3];
        for place in met {
            conds[*place] = true;
        }
        (Datetime::from_seconds(second), conds)
    }

    fn pattern(text: &str) -> Vec<super::Step> {
        pattern_of(text, Span::new(0, 0)).unwrap()
    }

    #[test]
    fn the_funnel_is_the_longest_prefix_one_chain_meets_inside_the_window() {
        // 1 at 0s, 2 at 100s, 3 at 500s; then a second chain 1 at 1000s,
        // 2 at 1100s. Window 300: the first chain reaches 2 (3 is 500s past
        // its start), the second reaches 2; best 2.
        let events = [
            event(0, &[0]),
            event(100, &[1]),
            event(500, &[2]),
            event(1000, &[0]),
            event(1100, &[1]),
        ];
        assert_eq!(
            funnel(&events, 300.0),
            Value::Number(tessari_types::Number::Integer(2))
        );
        // Window 600: the first chain completes.
        assert_eq!(
            funnel(&events, 600.0),
            Value::Number(tessari_types::Number::Integer(3))
        );
        // A chain must start at condition 1: events meeting only 2 and 3
        // reach nothing.
        let tail = [event(0, &[1]), event(10, &[2])];
        assert_eq!(
            funnel(&tail, 100.0),
            Value::Number(tessari_types::Number::Integer(0))
        );
        // Arrival order is not time order; the sort is the fold's own.
        let shuffled = [event(100, &[1]), event(0, &[0]), event(200, &[2])];
        assert_eq!(
            funnel(&shuffled, 300.0),
            Value::Number(tessari_types::Number::Integer(3))
        );
    }

    #[test]
    fn a_pattern_is_steps_with_bounds_and_nothing_else() {
        let steps = pattern("(1)(<1800)(2)(>60)(3)");
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].cond, 0);
        assert_eq!(steps[1].within, Some(1800.0));
        assert_eq!(steps[2].after, Some(60.0));
        for bad in [
            "",
            "(0)",
            "(x)",
            "(1)(<x)(2)",
            "(1)(<2)(<3)(2)",
            "(<5)",
            "(1)(>",
        ] {
            assert!(
                pattern_of(bad, Span::new(0, 0)).is_err(),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn a_match_is_a_chain_in_order_with_its_gaps_inside_the_bounds() {
        // 1 at 0s, 2 at 100s: gap 100, inside (<1800).
        let events = [event(0, &[0]), event(100, &[1])];
        assert_eq!(
            matched(&events, &pattern("(1)(<1800)(2)")),
            Value::Number(tessari_types::Number::Integer(1))
        );
        // The same events under (<50): the gap is too wide.
        assert_eq!(
            matched(&events, &pattern("(1)(<50)(2)")),
            Value::Number(tessari_types::Number::Integer(0))
        );
        // Under (>50): wide enough.
        assert_eq!(
            matched(&events, &pattern("(1)(>50)(2)")),
            Value::Number(tessari_types::Number::Integer(1))
        );
        // Order matters: 2 before 1 is no chain for (1)(2).
        let reversed = [event(0, &[1]), event(100, &[0])];
        assert_eq!(
            matched(&reversed, &pattern("(1)(2)")),
            Value::Number(tessari_types::Number::Integer(0))
        );
        // The earliest start is not the only one tried: a first event that
        // leaves no room under the upper bound is passed over.
        let tight = [event(0, &[0]), event(1000, &[0]), event(1100, &[1])];
        assert_eq!(
            matched(&tight, &pattern("(1)(<200)(2)")),
            Value::Number(tessari_types::Number::Integer(1))
        );
    }

    #[test]
    fn a_count_is_the_non_overlapping_chains_found_earliest_first() {
        // Two clean chains: (1,2) at 0/10 and at 100/110.
        let events = [
            event(0, &[0]),
            event(10, &[1]),
            event(100, &[0]),
            event(110, &[1]),
        ];
        assert_eq!(
            counted(&events, &pattern("(1)(2)")),
            Value::Number(tessari_types::Number::Integer(2))
        );
        // A 2 shared by two 1s is one chain, not two: the scan resumes past
        // the chain's last event.
        let shared = [event(0, &[0]), event(5, &[0]), event(10, &[1])];
        assert_eq!(
            counted(&shared, &pattern("(1)(2)")),
            Value::Number(tessari_types::Number::Integer(1))
        );
    }

    #[test]
    fn retention_anchors_every_later_condition_on_the_first() {
        // Two events: the first meets 1 and 3, the second meets 2 — the
        // anchor holds, 2 and 3 were both met inside the group.
        let events = [event(0, &[0, 2]), event(10, &[1])];
        assert_eq!(
            retained(&events),
            Value::Array(vec![
                Value::Number(tessari_types::Number::Integer(1)),
                Value::Number(tessari_types::Number::Integer(1)),
                Value::Number(tessari_types::Number::Integer(1)),
            ])
        );
        // No anchor: nothing later counts, whatever was met.
        let no_anchor = [event(0, &[1, 2])];
        assert_eq!(
            retained(&no_anchor),
            Value::Array(vec![
                Value::Number(tessari_types::Number::Integer(0)),
                Value::Number(tessari_types::Number::Integer(0)),
                Value::Number(tessari_types::Number::Integer(0)),
            ])
        );
        assert_eq!(retained(&[]), Value::None);
    }
}
