//! Where the memory an answer costs actually goes.
//!
//! # The question, and why it precedes any attempt to fix it
//!
//! The `range` workload established that reading fifty thousand records costs
//! about 1.4 KiB of resident memory per record answered, against records whose
//! stored payload is a couple of hundred bytes, and `docs/bgvql.md` §8 carries
//! that number as the reason a bounded answer is worth building. What neither
//! carries is where the difference goes — `ps` reports one number for the store,
//! the harness and the answer together, and cannot attribute.
//!
//! That matters because the two candidate explanations want opposite fixes. If
//! the cost is *holding many records at once*, bounding how many are held is the
//! fix. If it is a **per-record constant of the decoded form**, bounding the
//! count moves the total and leaves the constant exactly where it was — and an
//! answer that streamed would pay the same constant on every record in flight.
//!
//! # What this measures
//!
//! Four readings around one widest read, from the counting allocator rather than
//! from `ps`, with the answer deliberately still held at the third:
//!
//! - `before` — live bytes with the store settled and nothing read.
//! - `peak` — the highest live figure reached during the read.
//! - `held` — live bytes with the answer in hand.
//! - `after` — live bytes once the answer is dropped.
//!
//! `held - after` over the records answered is what one record costs in the form
//! the caller receives. `peak - held` over the same is what the read builds and
//! throws away per record: a large figure there means the pipeline holds the
//! answer more than once, which is a different defect from the answer being
//! expensive and is repaired differently. `after - before` is the store's own
//! growth across the read, and it has to be small or the first two figures are
//! attributing to the answer something that belongs to the store.
//!
//! Then the isolating half: the same record shape built with no store involved,
//! at none, one, two, four and eight fields. A cost that barely moves as fields
//! are added is a fixed allocation per record and not the content of one — and
//! the step from none to one says what that allocation is, which turns the
//! explanation into a measurement. This is the claim the whole workload exists
//! to settle either way.

use std::collections::BTreeMap;

use bgv_db::{Db, Outcome};
use bgv_db_types::{Number, RecordId, Value};

use crate::counting;
use crate::ranges::{NOTE, RECORDS, spans};
use crate::samples::Report;
use crate::workload::{Failable, resident_bytes};

/// How many records the isolating measurement builds at each field count.
///
/// Enough that the per-record figure is not dominated by the vector's own
/// header, and small enough to be instant.
const SHAPED: usize = 20_000;

/// The field counts the isolating measurement uses.
///
/// Two is the record the store actually holds. The others are there to turn an
/// explanation into a measurement. Zero holds no map at all, so it is the cost
/// of the pair alone; the step from zero to one is whatever a map costs before
/// it holds anything worth measuring; and four and eight answer whether the cost
/// after that tracks the content or ignores it.
const WIDTHS: &[usize] = &[0, 1, 2, 4, 8];

/// The size of the block the self-check allocates.
const CHECK: usize = 1 << 20;

/// Where the memory an answer costs goes.
///
/// # Errors
///
/// Returns an error when a statement fails or the store cannot be prepared.
pub fn memory(db: &Db) -> Failable<Vec<Report>> {
    let mut reports = vec![self_check()];

    let written = spans(db, RECORDS)?;
    reports.push(written.summarise("memory-write"));

    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench;")?;

    let before = counting::live();
    counting::reset_peak();
    let outcome = session.run(&format!(
        "SELECT * FROM spans WHERE n >= 0 AND n < {RECORDS};"
    ))?;
    let peak = counting::peak();
    let held = counting::live();

    // Read while the answer is still in hand, for the same reason the readings
    // are taken at all: the figure being sought is what the caller is holding,
    // and it stops existing the moment the answer is dropped.
    let (answered, path) = match outcome.last() {
        Some(Outcome::Records { records, path }) => (records.len(), path.name()),
        _ => (0, "nothing"),
    };
    drop(outcome);
    let after = counting::live();

    // The access path is reported for the reason the `range` workload reports
    // it: if a planner change stopped serving this from the index, every figure
    // below would still be a real measurement of something else.
    reports.push(Report::measurement(
        "  served by",
        &format!("{path} — {answered} record(s) of {RECORDS} asked"),
    ));
    reports.push(kib("  before the read", before));
    reports.push(kib("  peak during", peak));
    reports.push(kib("  held, answer in hand", held));
    reports.push(kib("  after the answer is dropped", after));

    reports.push(Report::measurement(
        "  the answer, per record",
        &per_record(held.saturating_sub(after), answered),
    ));
    reports.push(Report::measurement(
        "  built and discarded, per record",
        &per_record(peak.saturating_sub(held), answered),
    ));
    reports.push(Report::measurement(
        "  the store's own growth",
        &format!(
            "{} KiB — attributed to neither figure above",
            after.saturating_sub(before) / 1024
        ),
    ));
    if let Some(resident) = resident_bytes() {
        // Beside the counted figure rather than instead of it. A wide
        // disagreement between the two is a finding about the allocator's
        // rounding and bookkeeping, not about the store, and it is only visible
        // when both are printed.
        reports.push(Report::measurement(
            "  resident, for comparison",
            &format!(
                "{} KiB by `ps` against {} KiB counted",
                resident / 1024,
                held / 1024
            ),
        ));
    }

    reports.extend(shapes());
    Ok(reports)
}

/// The same record shape, built with no store involved, at several widths.
///
/// The decisive comparison. If the two-field figure lands near the answer's
/// per-record cost, then what the read is paying for is the decoded value and
/// not the reading; and if the figure barely moves between two fields and eight,
/// then it is one allocation per record whose size is set by the type rather
/// than by the record.
fn shapes() -> Vec<Report> {
    let mut reports = vec![Report::measurement(
        "  a decoded record, no store involved",
        &format!(
            "{SHAPED} records; a value is {} bytes, a pair {} bytes",
            size_of::<Value>(),
            size_of::<(RecordId, Value)>()
        ),
    )];
    for fields in WIDTHS.iter().copied() {
        reports.push(Report::measurement(
            &format!("    {fields} field(s), per record"),
            &format!("{} bytes", shaped(fields)),
        ));
    }
    reports
}

/// What one record of `fields` fields costs, held in the answer's own shape.
fn shaped(fields: usize) -> usize {
    let before = counting::live();
    let held: Vec<(RecordId, Value)> = (0..SHAPED)
        .map(|n| {
            (
                RecordId::Int(i64::try_from(n).unwrap_or(0)),
                record(fields, n),
            )
        })
        .collect();
    let cost = counting::live().saturating_sub(before);
    drop(held);
    cost.checked_div(SHAPED).unwrap_or(0)
}

/// One record of `fields` fields: the store's two, then short ones after.
fn record(fields: usize, n: usize) -> Value {
    let mut object = BTreeMap::new();
    if fields > 0 {
        object.insert(
            "n".to_owned(),
            Value::Number(Number::Integer(i64::try_from(n).unwrap_or(0))),
        );
    }
    if fields > 1 {
        object.insert("note".to_owned(), Value::String(NOTE.to_owned()));
    }
    for extra in 2..fields {
        object.insert(format!("f{extra}"), Value::Number(Number::Integer(0)));
    }
    Value::Object(object)
}

/// Prove the counter before anything is derived from it.
///
/// The instrument is the one thing in this workload that cannot be taken on
/// trust: every figure below is a difference of two of its readings, so a
/// counter that under-counts reports a per-record cost that is wrong in the
/// direction that makes the answer look cheap. This runs on every run rather
/// than only in a test, because the test lives behind the same feature flag as
/// the counter and a check that runs in a build nobody uses is not a check.
fn self_check() -> Report {
    let before = counting::live();
    let held = vec![0_u8; CHECK];
    let moved = counting::live().saturating_sub(before);
    drop(held);
    let released = counting::live();
    Report::measurement(
        "counter self-check",
        &format!(
            "{} KiB allocated moved the total by {} KiB and left {} KiB — {}",
            CHECK / 1024,
            moved / 1024,
            released.saturating_sub(before) / 1024,
            if moved >= CHECK && released.saturating_sub(before) < CHECK {
                "the counter tracks"
            } else {
                "THE COUNTER DOES NOT TRACK — every figure below is void"
            }
        ),
    )
}

/// A reading, in the unit every other memory figure in this harness uses.
fn kib(what: &str, bytes: usize) -> Report {
    Report::measurement(what, &format!("{} KiB", bytes / 1024))
}

/// A total spread over the records it covers.
fn per_record(bytes: usize, records: usize) -> String {
    match bytes.checked_div(records) {
        Some(each) => format!(
            "{each} bytes ({} KiB over {records} record(s))",
            bytes / 1024
        ),
        None => "no records answered".to_owned(),
    }
}
