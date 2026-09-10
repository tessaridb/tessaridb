//! What a claim costs, and what makes it cost more.
//!
//! # The number this exists to find or refute
//!
//! The queue's design states the cost in one line and leaves it to be measured:
//! **the claim walk is O(held + dead-lettered), and only the held half heals
//! itself.** A `CLAIM` steps through the table in identity order and passes over
//! every record it may not hand out, so the price of a claim is set by how much
//! unclaimable work sits in front of the first claimable record.
//!
//! The two halves of that prefix are the same cost and a different consequence,
//! which is why both are measured here side by side rather than one being
//! inferred from the other:
//!
//! - a **held** record is skipped until its deadline passes, and the deadline
//!   passes on its own, so its contribution to the walk expires without anybody
//!   doing anything;
//! - a **dead-lettered** record — one that has reached the queue's `ATTEMPTS`
//!   ceiling — is skipped by the same walk and never becomes claimable again, so
//!   its contribution is **permanent** until an operator deletes it.
//!
//! If the two curves are the same, the walk does not care *why* a record is
//! skipped, and the design's asymmetry is entirely about what happens next
//! rather than about what a claim pays. That is the finding this workload is
//! shaped to produce, and it is the one that tells an operator whether the
//! retention statement in the design's §7 is housekeeping or a requirement.
//!
//! # How a prefix is built, and why it is built by claiming
//!
//! Both prefixes are made by the ordinary statement. Held records are claimed
//! under a long timeout and simply left; dead-lettered records are claimed under
//! `ATTEMPTS 1` and a lapsing hold, so one hand-out spends the record's only
//! attempt. Writing the two engine fields directly would be faster and would
//! measure a state the store cannot reach.
//!
//! # What the throughput rows are for
//!
//! `CLAIM` takes a count, and the design's answer to contention is to claim a
//! batch. The last two rows are the same clean queue drained one record at a
//! time and a hundred at a time, which is the number behind that advice.

use std::time::Instant;

use tessaridb::Db;

use crate::samples::{Report, Samples};
use crate::workload::{Failable, prepared};

/// How deep the unclaimable prefix is made, in records.
///
/// Four points rather than two because the claim is that the cost is **linear**
/// in the prefix, and two points cannot tell a line from a curve.
const DEPTHS: [u64; 4] = [0, 1_000, 5_000, 20_000];

/// How many claims are timed at each depth.
///
/// In records rather than in samples, because it is both: the timed claims are
/// what consumes the claimable records seeded behind each prefix, so one number
/// answers "how many samples" and "how much work to seed behind the prefix", and
/// two constants for one quantity is how the two drift apart.
const SAMPLES: u64 = 20;

/// The largest number of records one statement may claim.
///
/// The store's own ceiling, spelled here because the prefixes are built with it:
/// a prefix is claimed in whole batches, and a batch above this is refused.
const CLAIM_CEILING: u64 = 500;

/// How many records the throughput rows drain.
const DRAIN: u64 = 2_000;

/// How many records a batched claim takes at once.
const BATCH: u64 = 100;

/// Claim latency behind a prefix of held and of dead-lettered records, and the
/// throughput of a claim taken one at a time against one taken in a batch.
///
/// # Errors
///
/// Returns whatever the store returns: this harness reports failures rather than
/// timing around them.
pub fn queue(db: &Db) -> Failable<Vec<Report>> {
    prepared(db)?;
    let mut session = db.session();
    session.run("USE NAMESPACE bench; USE DATABASE bench;")?;

    let mut reports = vec![Report::measurement(
        "corpus",
        &format!(
            "prefixes of {DEPTHS:?} records, {SAMPLES} claims timed at each; \
             drain of {DRAIN} at 1 and at {BATCH}"
        ),
    )];

    for depth in DEPTHS {
        // A queue per depth, so a prefix built for one measurement is never the
        // prefix another one inherits. The claimable records behind the prefix
        // are what the timed claims consume.
        let held_name = format!("held_{depth}");
        session.run(&format!("DEFINE QUEUE {held_name} TIMEOUT 3600s;"))?;
        seed(&mut session, &held_name, depth.saturating_add(SAMPLES))?;
        // Claimed and left. An hour is far longer than this workload runs, so
        // nothing in the prefix lapses while it is being measured.
        take(&mut session, &held_name, depth)?;
        reports.push(timed_claims(
            &mut session,
            &held_name,
            &format!("claim behind {depth} held"),
        )?);

        // The same prefix, unclaimable for the other reason. `ATTEMPTS 1` with a
        // hold of no length means the deadline is never what stops a record
        // being handed out again — the spent attempt is.
        let dead_name = format!("dead_{depth}");
        session.run(&format!("DEFINE QUEUE {dead_name} TIMEOUT 1ns ATTEMPTS 1;"))?;
        seed(&mut session, &dead_name, depth.saturating_add(SAMPLES))?;
        take(&mut session, &dead_name, depth)?;
        reports.push(timed_claims(
            &mut session,
            &dead_name,
            &format!("claim behind {depth} dead-lettered"),
        )?);
    }

    // Throughput on a queue that stays clean, which is the state a healthy queue
    // is in and the state the rows above are deliberately not in.
    //
    // **Each claim is finished before the next one is timed, and that is not a
    // detail.** A drain that claims without deleting leaves every record it took
    // held at the head, so the prefix grows under the measurement and the row
    // reports the average of a walk that got steadily longer — which is what the
    // first version of this workload did, and it produced a "clean queue"
    // throughput identical to the thousand-held row above. The delete is a
    // worker finishing its work, and it is outside the timer because what is
    // being measured is the claim.
    session.run("DEFINE QUEUE drained_one TIMEOUT 3600s;")?;
    seed(&mut session, "drained_one", DRAIN)?;
    let mut one = Samples::with_capacity(usize::try_from(DRAIN).unwrap_or(usize::MAX));
    for _ in 0..DRAIN {
        let started = Instant::now();
        let taken = session.run("CLAIM FROM drained_one;")?;
        one.push(started.elapsed());
        finish(&mut session, "drained_one", &handed_out(taken))?;
    }
    reports.push(one.summarise("drain one at a time"));

    session.run("DEFINE QUEUE drained_batch TIMEOUT 3600s;")?;
    seed(&mut session, "drained_batch", DRAIN)?;
    let batches = DRAIN / BATCH;
    let mut batched = Samples::with_capacity(usize::try_from(batches).unwrap_or(usize::MAX));
    for _ in 0..batches {
        let started = Instant::now();
        let taken = session.run(&format!("CLAIM {BATCH} FROM drained_batch;"))?;
        batched.push(started.elapsed());
        finish(&mut session, "drained_batch", &handed_out(taken))?;
    }
    // The row counts **statements**, so its throughput is claims per second and
    // not records per second. Divide the latency by the batch size for the
    // per-record figure, which is the comparison the row beside it invites.
    reports.push(batched.summarise(&format!("drain {BATCH} at a time")));

    Ok(reports)
}

/// Fill a queue with `count` pieces of work.
fn seed(session: &mut tessaridb::Session<'_>, queue: &str, count: u64) -> Failable<()> {
    for n in 0..count {
        session.run(&format!("CREATE {queue}:{n} = {{ url: 'j{n}' }};"))?;
    }
    Ok(())
}

/// Hand out the first `count` records, whatever that leaves them.
///
/// In whole statements of at most the store's ceiling, because that is the only
/// way a caller can take them.
fn take(session: &mut tessaridb::Session<'_>, queue: &str, count: u64) -> Failable<()> {
    let mut taken = 0_u64;
    while taken < count {
        let asking = CLAIM_CEILING.min(count.saturating_sub(taken));
        session.run(&format!("CLAIM {asking} FROM {queue};"))?;
        taken = taken.saturating_add(asking);
    }
    Ok(())
}

/// Time `SAMPLES` single claims against a queue and summarise them.
fn timed_claims(
    session: &mut tessaridb::Session<'_>,
    queue: &str,
    phase: &str,
) -> Failable<Report> {
    let mut samples = Samples::with_capacity(usize::try_from(SAMPLES).unwrap_or(usize::MAX));
    for _ in 0..SAMPLES {
        let started = Instant::now();
        session.run(&format!("CLAIM FROM {queue};"))?;
        samples.push(started.elapsed());
    }
    Ok(samples.summarise(phase))
}

/// The identities one answer handed out.
fn handed_out(outcomes: Vec<tessaridb::Outcome>) -> Vec<tessari_types::RecordId> {
    outcomes
        .first()
        .and_then(tessaridb::Outcome::records)
        .map(|records| records.iter().map(|(id, _)| id.clone()).collect())
        .unwrap_or_default()
}

/// Finish work, which in this language is deleting the record.
fn finish(
    session: &mut tessaridb::Session<'_>,
    queue: &str,
    taken: &[tessari_types::RecordId],
) -> Failable<()> {
    for id in taken {
        session.run(&format!("DELETE {queue}:{};", id.to_literal()))?;
    }
    Ok(())
}
