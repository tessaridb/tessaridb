//! Who holds a queued record, until when, and what a claim may take.

use super::{Claimant, attempts_of};
use crate::error::{Error, Result};
use crate::outcome::{AccessPath, Outcome};
use crate::plan::Plan;
use crate::session::Consumer;
use std::collections::BTreeMap;
use tessari_encoding::decode_payload;
use tessari_ql::Span;
use tessari_storage::{
    CLAIMED_BY_CONSUMER, CLAIMED_BY_INSTANCE, Catalog, QUEUE_ATTEMPTS, QUEUE_CLAIMED_BY,
    QUEUE_CLAIMED_UNTIL, QueueDeclaration, RecordAddress, TableDefinition, TableKind, Transaction,
};
use tessari_types::{Datetime, Duration, Value};

/// The claimant a record carries, when it carries one.
///
/// A record whose `claimed_by` is malformed reads as **unheld** rather than
/// raising: the field is the engine's and a caller cannot write it, so a shape
/// that is wrong here is this engine's own bug, and failing a release because of
/// it would leave the work stuck with no way for an operator to free it.
pub(crate) fn claimant_of(fields: &BTreeMap<String, Value>) -> Option<Claimant> {
    let Some(Value::Object(held)) = fields.get(QUEUE_CLAIMED_BY) else {
        return None;
    };
    let text = |route: &str| match held.get(route) {
        Some(Value::String(found)) => Some(found.clone()),
        _ => None,
    };
    Some(Claimant {
        name: text(CLAIMED_BY_CONSUMER)?,
        instance: text(CLAIMED_BY_INSTANCE)?,
    })
}

/// Record who is taking this hold, when the session said who it is.
///
/// Writes nothing when it did not, which is what keeps the field's absence
/// meaning *nobody said* rather than a default that is itself a claim.
pub(crate) fn mark_claimant(fields: &mut BTreeMap<String, Value>, claimant: Option<&Consumer>) {
    let Some(claimant) = claimant else {
        return;
    };
    let mut held = BTreeMap::new();
    held.insert(
        CLAIMED_BY_CONSUMER.to_owned(),
        Value::from(claimant.name.as_str()),
    );
    held.insert(
        CLAIMED_BY_INSTANCE.to_owned(),
        Value::from(claimant.instance.as_str()),
    );
    fields.insert(QUEUE_CLAIMED_BY.to_owned(), Value::Object(held));
}

/// The answer a targeted claim gives when the record is not claimable.
///
/// No records and no error — the same shape a successful claim answers with, so
/// a caller reads one thing either way and the ordinary busy case is not a
/// failure.
pub(crate) fn nothing_claimed(table: &str) -> Outcome {
    Outcome::Records {
        records: Vec::new(),
        plan: Plan::new(AccessPath::Record).on(table),
        notes: Vec::new(),
        suggestion: None,
        only: false,
    }
}

/// The declaration of the queue a statement named, or the refusal.
pub(crate) fn queue_declaration(
    transaction: &mut Transaction<'_>,
    table: tessari_types::TableId,
    named: &str,
    span: Span,
) -> Result<QueueDeclaration> {
    let definition: Option<TableDefinition> = Catalog::new(transaction).table(table)?;
    match definition.map(|found| found.kind) {
        Some(TableKind::Queue(declared)) => Ok(declared),
        _ => Err(Error::NotAQueue {
            table: named.to_owned(),
            span,
        }),
    }
}

/// When a claim taken at `now` lapses.
pub(crate) fn deadline(now: Datetime, declared: &QueueDeclaration, span: Span) -> Result<Datetime> {
    later(now, declared.timeout, span)
}

/// `now` moved on by `by`, refused when that is past what a datetime can hold.
pub(crate) fn later(now: Datetime, by: Duration, span: Span) -> Result<Datetime> {
    let seconds = now
        .seconds()
        .checked_add(by.seconds())
        .ok_or(Error::ClaimDeadlineUnreachable { span })?;
    let nanos = now.nanos().saturating_add(by.nanos());
    let (seconds, nanos) = if nanos >= 1_000_000_000 {
        (
            seconds
                .checked_add(1)
                .ok_or(Error::ClaimDeadlineUnreachable { span })?,
            nanos.saturating_sub(1_000_000_000),
        )
    } else {
        (seconds, nanos)
    };
    Datetime::new(seconds, nanos).ok_or(Error::ClaimDeadlineUnreachable { span })
}

/// Whether this record may be handed out now.
///
/// Two questions, and the order matters only for readability: a record whose
/// attempts are spent is never claimable however long ago its hold lapsed, and a
/// record still held is not claimable however few attempts it has had.
pub(crate) fn claimable(
    fields: &BTreeMap<String, Value>,
    now: Datetime,
    declared: &QueueDeclaration,
) -> bool {
    if let Some(ceiling) = declared.attempts
        && attempts_of(fields) >= i64::from(ceiling)
    {
        return false;
    }
    // Delayed delivery (G055 C8): an instant after now is a record that is
    // not due yet. Anything that is not an instant is no delay — a value the
    // writer got wrong must not hold a record back forever with nothing in an
    // error state.
    if let Some(field) = &declared.not_before
        && let Some(Value::Datetime(due)) = fields.get(field)
        && *due > now
    {
        return false;
    }
    match fields.get(QUEUE_CLAIMED_UNTIL) {
        // Nothing holds it.
        None | Some(Value::None) => true,
        // Held until the stored instant, which is the whole expiry mechanism:
        // the comparison is the sweep.
        Some(Value::Datetime(until)) => *until <= now,
        // A hold this build cannot read is treated as held rather than as
        // absent. The two failures are not symmetric — reading it as absent
        // hands live work to a second worker, while reading it as held delays
        // the record until somebody looks at it.
        Some(_) => false,
    }
}

/// Refuse a caller's write that sets a field only the engine may set.
///
/// Called from the one funnel every caller-driven record write passes through,
/// so this is one rule in one place rather than a check each write path has to
/// remember. The engine's own two writes go through the sibling that skips it,
/// and they are the only two.
///
/// The reasoning is the bucket's, in a second place: engine metadata a caller
/// can write is metadata that can lie, and a hold whose deadline the holder
/// chose is not a hold.
///
/// # Errors
///
/// Returns [`Error::QueueFieldIsTheEngines`] naming the field, so a caller whose
/// payload happens to use one of these names is told exactly what happened
/// rather than silently losing it (Q-461).
pub(crate) fn hold_engine_fields(
    transaction: &mut Transaction<'_>,
    address: &RecordAddress,
    payload: &mut Value,
    span: Span,
) -> Result<()> {
    let Value::Object(fields) = payload else {
        return Ok(());
    };
    // No early return on "the payload mentions none of them" any more, and that
    // early return was the whole defect: a payload mentioning none of them is
    // exactly a whole-record write, which is the one that drops all three.
    let is_queue = Catalog::new(transaction)
        .table(address.table)?
        .is_some_and(|found| matches!(found.kind, TableKind::Queue(_)));
    if !is_queue {
        return Ok(());
    }

    // What the record holds now, because the payload above is not what the
    // caller said. Every caller-driven write funnels through here as the whole
    // record about to be stored, and for `UPDATE ... SET` that is the merge of
    // the caller's assignments over what was already there — so on a held
    // record it carries all three of these whether or not the caller mentioned
    // any. Judging the payload alone therefore refused a worker who wrote a
    // field of its own on work it had just taken, naming a field that worker
    // had never typed, which made a claimed record unwritable and a claim
    // pointless.
    //
    // The rule is that a caller may not INTRODUCE or CHANGE one of these.
    // Carrying one forward unchanged is not writing it: the value that reaches
    // storage is the value the engine put there, so nothing a caller could lie
    // about has moved.
    let stored = match transaction.get(address)? {
        Some(bytes) => match decode_payload(&bytes)? {
            Value::Object(held) => held,
            _ => BTreeMap::new(),
        },
        None => BTreeMap::new(),
    };
    let supplied = |name: &str| {
        fields
            .get(name)
            .is_some_and(|value| stored.get(name) != Some(value))
    };

    let refused = if supplied(QUEUE_CLAIMED_UNTIL) {
        Some(QUEUE_CLAIMED_UNTIL)
    } else if supplied(QUEUE_ATTEMPTS) {
        Some(QUEUE_ATTEMPTS)
    } else if supplied(QUEUE_CLAIMED_BY) {
        // The strongest of the three to refuse. The other two are the engine's
        // bookkeeping; this one is an ASSERTION ABOUT WHO, and a caller able to
        // write it could sign a hold with another consumer's name and then have
        // that consumer's `RELEASE ALL` drop it.
        Some(QUEUE_CLAIMED_BY)
    } else {
        None
    };
    if let Some(field) = refused {
        return Err(Error::QueueFieldIsTheEngines { field, span });
    }

    // Carried forward, which is the other half of the same rule and the half
    // that was missing. A caller may not introduce or change one of these; it
    // follows that a caller may not REMOVE one either, and a whole-record
    // `UPDATE t:1 = { … }` removes every field it does not mention. Refusing
    // such a write would be the other reading and is the wrong one: the
    // conditional whole-record write is exactly the compare-and-set a consumer
    // holding work performs, so refusing it would take away the capability this
    // clause was added for.
    //
    // Two things this protects, and they are not the same thing. The **hold**:
    // a record rewritten by its holder came back unheld, so the work was
    // claimable again while that holder still believed it held it — no error at
    // any point, which is the failure the conditional update was built to
    // prevent, reached by a door nobody had checked. And the **ceiling**: a
    // caller that may write the record could clear `attempts`, so a record that
    // had poisoned three workers could be recycled by the fourth. A count a
    // caller can reset is not a ceiling.
    //
    // `UPDATE … SET` never showed this, because the payload reaching here is
    // the merge over the stored record and therefore already carries all three.
    // The two edit forms disagreed silently; now they agree.
    for name in [QUEUE_CLAIMED_UNTIL, QUEUE_ATTEMPTS, QUEUE_CLAIMED_BY] {
        if !fields.contains_key(name)
            && let Some(kept) = stored.get(name)
        {
            fields.insert(name.to_owned(), kept.clone());
        }
    }
    Ok(())
}
