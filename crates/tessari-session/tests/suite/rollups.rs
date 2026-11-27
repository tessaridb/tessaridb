//! `DEFINE ROLLUP` — per-window aggregates of an event-time series, kept exact
//! by the writing transaction (ADR-0088 §6, G044 C7). The oracle is the same
//! aggregate recomputed from the raw series with `GROUP BY` after every batch.

#![allow(clippy::panic, clippy::unwrap_used, clippy::arithmetic_side_effects)]

use std::collections::BTreeMap;
use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const ROLLUP: &str = "DEFINE ROLLUP hourly FROM readings WINDOW 1h BY sensor \
     COMPUTE count(*) AS n, count(v) AS nv, sum(v) AS total, min(v) AS low, max(v) AS high \
     RETAIN 36500d;";

const RECOMPUTED: &str = "SELECT sensor, time::bucket(at, 1h) AS window, count(*) AS n, \
     count(v) AS nv, sum(v) AS total, min(v) AS low, max(v) AS high FROM readings \
     GROUP BY sensor, time::bucket(at, 1h);";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap()
}

fn opened(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE SERIES readings RETAIN 36500d TIME at;",
        )
        .unwrap();
    session
}

/// Every row a read answered, keyed by `(sensor, window)`, without identities.
fn rows(
    session: &mut Session<'_>,
    read: &str,
) -> BTreeMap<(String, String), BTreeMap<String, Value>> {
    let Outcome::Records { records, .. } = session.run_now(read).unwrap().pop().unwrap() else {
        panic!("a read answers with records");
    };
    records
        .into_iter()
        .map(|(_, row)| {
            let Value::Object(fields) = row else {
                panic!("a row is an object")
            };
            let key = (fields["sensor"].to_string(), fields["window"].to_string());
            (key, fields)
        })
        .collect()
}

struct Draw(u64);

impl Draw {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below
    }
}

/// One raw reading at minute `minute` of the day, `v` absent one time in five.
fn reading(draw: &mut Draw) -> String {
    let minute = draw.next(600);
    let v = if draw.next(5) == 0 {
        String::new()
    } else {
        format!(", v: {}", draw.next(100))
    };
    format!(
        "CREATE readings = {{ sensor: 's{}', at: datetime '2026-09-29T{:02}:{:02}:{:02}Z'{v} }};",
        draw.next(3),
        minute / 60,
        minute % 60,
        draw.next(60)
    )
}

#[test]
fn a_rollup_equals_its_recomputation_after_every_batch() {
    let store = store();
    let mut session = opened(&store);
    let mut draw = Draw(0x0bad_5eed);
    let mut ids: Vec<String> = Vec::new();
    // Written before the rollup exists, so the backfill is what folds these.
    for _ in 0..40 {
        if let Some(Outcome::Keys(keys)) = session.run_now(&reading(&mut draw)).unwrap().pop() {
            ids.push(keys[0].to_literal());
        }
    }
    session.run_now(ROLLUP).unwrap();
    assert_eq!(
        rows(&mut session, "SELECT * FROM hourly;"),
        rows(&mut session, RECOMPUTED)
    );

    for batch in 0..200 {
        let mut script = String::from("BEGIN;");
        for _ in 0..(draw.next(4) + 1) {
            script.push(' ');
            script.push_str(&reading(&mut draw));
        }
        // A replacement and a removal, each recomputing its window.
        if !ids.is_empty() && draw.next(2) == 0 {
            let id = &ids[usize::try_from(draw.next(ids.len() as u64)).unwrap()];
            script.push_str(&format!(
                " UPDATE readings:{id} SET v = {};",
                draw.next(100)
            ));
        }
        if !ids.is_empty() && draw.next(3) == 0 {
            let at = usize::try_from(draw.next(ids.len() as u64)).unwrap();
            let id = ids.swap_remove(at);
            script.push_str(&format!(" DELETE readings:{id};"));
        }
        script.push_str(" COMMIT;");
        for outcome in session.run_now(&script).unwrap() {
            if let Outcome::Keys(keys) = outcome {
                ids.extend(keys.iter().map(tessari_types::RecordId::to_literal));
            }
        }
        assert_eq!(
            rows(&mut session, "SELECT * FROM hourly;"),
            rows(&mut session, RECOMPUTED),
            "batch {batch}"
        );
    }
}

#[test]
fn a_write_open_across_a_declaration_retries_and_is_then_folded() {
    let store = store();
    let mut writer = opened(&store);
    let mut declarer = Session::new(&store);
    declarer
        .run_now("USE NAMESPACE t; USE DATABASE d;")
        .unwrap();
    // The raw write's transaction is open while the declaration commits.
    let straddled: tessari_session::Result<()> = writer
        .atomically(async |work| {
            work.run_with(
                "CREATE readings = { sensor: 's0', v: 5, at: datetime '2026-09-29T10:00:00Z' };",
                &tessari_session::Parameters::new(),
            )
            .await?;
            declarer.run(ROLLUP).await?;
            Ok(())
        })
        .now();
    let refused = straddled.unwrap_err();
    assert!(
        matches!(
            &refused,
            tessari_session::Error::Store(tessari_storage::Error::Conflict { .. })
        ),
        "{refused:?}"
    );
    writer
        .run_now("CREATE readings = { sensor: 's0', v: 5, at: datetime '2026-09-29T10:00:00Z' };")
        .unwrap();
    assert_eq!(
        rows(&mut writer, "SELECT * FROM hourly;"),
        rows(&mut writer, RECOMPUTED)
    );
}

#[test]
fn what_a_rollup_cannot_keep_or_be_asked_is_refused() {
    let store = store();
    let mut session = opened(&store);
    let refused =
        |session: &mut Session<'_>, script: &str| session.run_now(script).unwrap_err().to_string();
    let mean = refused(
        &mut session,
        "DEFINE ROLLUP m FROM readings WINDOW 1h COMPUTE mean(v) AS avg RETAIN 1d;",
    );
    assert!(mean.contains("divide"), "{mean}");
    session
        .run_now("DEFINE SERIES arrivals RETAIN 1d;")
        .unwrap();
    let plain = refused(
        &mut session,
        "DEFINE ROLLUP a FROM arrivals WINDOW 1h COMPUTE count(*) AS n RETAIN 1d;",
    );
    assert!(plain.contains("declared with `TIME`"), "{plain}");
    let inside = refused(&mut session, &format!("BEGIN; {ROLLUP}"));
    assert!(inside.contains("outside `BEGIN"), "{inside}");
    session.run_now("CANCEL;").ok();
    session.run_now(ROLLUP).unwrap();
    let written = refused(
        &mut session,
        "CREATE hourly = { window: datetime '2026-09-29T10:00:00Z', n: 1 };",
    );
    assert!(written.contains("is a rollup"), "{written}");
    let dropped = refused(&mut session, "DROP SERIES readings;");
    assert!(dropped.contains("drop them first"), "{dropped}");
    session
        .run_now("DROP ROLLUP hourly; DROP SERIES readings;")
        .unwrap();
}

/// A float `sum` kept one insert at a time answers the exact total, as the
/// recomputation does (ADR-0114, Q-927): the row carries the exact state, not
/// the rounded total re-entered as one value.
#[test]
fn a_float_sum_kept_insert_by_insert_is_the_exact_total() {
    let store = store();
    let mut session = opened(&store);
    session.run_now(ROLLUP).unwrap();
    // 1e16 + 1.0 rounds back to 1e16, so a running rounded total loses every
    // 1.0 and ends at 0 where the exact total is 4.
    for v in ["1e16", "1.0", "1.0", "1.0", "1.0", "-1e16"] {
        session
            .run_now(&format!(
                "CREATE readings = {{ sensor: 'f', v: {v}, at: datetime '2026-09-29T10:00:00Z' }};"
            ))
            .unwrap();
    }
    let kept = rows(&mut session, "SELECT * FROM hourly;");
    let (_, row) = kept.iter().next().unwrap();
    assert_eq!(row["total"], Value::from(4.0_f64), "{row:?}");
    assert_eq!(kept, rows(&mut session, RECOMPUTED));
    // And one more insert into the same window keeps it exact.
    session
        .run_now("CREATE readings = { sensor: 'f', v: 0.5, at: datetime '2026-09-29T10:30:00Z' };")
        .unwrap();
    let kept = rows(&mut session, "SELECT * FROM hourly;");
    assert_eq!(kept.values().next().unwrap()["total"], Value::from(4.5_f64));
    assert_eq!(kept, rows(&mut session, RECOMPUTED));
    // The key's writes move on to the next hour, and then one arrives late for
    // the first: its row is no longer the one whose state is kept.
    for v in ["1e16", "1.0", "-1e16"] {
        session
            .run_now(&format!(
                "CREATE readings = {{ sensor: 'f', v: {v}, at: datetime '2026-09-29T11:00:00Z' }};"
            ))
            .unwrap();
    }
    for v in ["1e16", "1.0", "1.0", "-1e16"] {
        session
            .run_now(&format!(
                "CREATE readings = {{ sensor: 'f', v: {v}, at: datetime '2026-09-29T10:45:00Z' }};"
            ))
            .unwrap();
    }
    let kept = rows(&mut session, "SELECT * FROM hourly;");
    let totals: Vec<&Value> = kept.values().map(|row| &row["total"]).collect();
    assert_eq!(totals, [&Value::from(6.5_f64), &Value::from(1.0_f64)]);
    assert_eq!(kept, rows(&mut session, RECOMPUTED));
}

#[test]
fn dropping_a_rollup_takes_its_exact_sums() {
    use sha2::{Digest, Sha256};

    let store = store();
    let mut session = opened(&store);
    session.run_now(ROLLUP).unwrap();
    session
        .run_now("CREATE readings = { sensor: 's0', v: 1.5, at: datetime '2026-09-29T10:00:00Z' };")
        .unwrap();
    let rollup = {
        let mut transaction = store.begin().now().unwrap();
        let catalog = tessari_storage::Catalog::new(&mut transaction);
        let namespace = catalog.namespace_id("t").now().unwrap().unwrap();
        let database = catalog.database_id(namespace, "d").now().unwrap().unwrap();
        let rollup = catalog
            .table_id(namespace, database, "hourly")
            .now()
            .unwrap()
            .unwrap();
        transaction.rollback();
        rollup
    };
    // The key's state is kept under a digest of the key.
    let key = Sha256::digest(tessari_encoding::encode_payload(&Value::from("s0")).into_bytes());
    let kept = |store: &Store| {
        let transaction = store.begin().now().unwrap();
        transaction.rollup_state(rollup, &key[..16]).now().unwrap()
    };
    // Pinned before the drop, so the absence after it is not vacuous.
    assert!(kept(&store).is_some());
    session.run_now("DROP ROLLUP hourly;").unwrap();
    assert_eq!(kept(&store), None);
}

/// `KEEP RAW FOR … ROLL UP BY …` — the age-triggered pass replaces each
/// finished aged window's raw points with one rolled row, idempotently.
#[test]
fn keep_raw_rolls_aged_windows_and_a_rerun_changes_nothing() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE SERIES readings RETAIN 36500d TIME at \
             KEEP RAW FOR 1s ROLL UP BY 1m COMPUTE count(*) AS n, sum(v) AS total;",
        )
        .unwrap();
    // Two finished windows far past the one-second raw horizon.
    session
        .run_now(
            "CREATE readings = { at: datetime '2026-11-01T10:00:10Z', v: 1 };\n\
             CREATE readings = { at: datetime '2026-11-01T10:00:20Z', v: 2 };\n\
             CREATE readings = { at: datetime '2026-11-01T10:00:30Z', v: 3 };\n\
             CREATE readings = { at: datetime '2026-11-01T10:01:10Z', v: 10 };",
        )
        .unwrap();
    let rolled = tessari_session::roll_up_aged_series(&store).now().unwrap();
    assert_eq!(rolled.windows, 2, "{rolled:?}");
    assert_eq!(rolled.points, 4, "{rolled:?}");
    // The raw points are gone; each window answers as one rolled row.
    let all = |session: &mut Session<'_>| -> Vec<BTreeMap<String, Value>> {
        let Outcome::Records { records, .. } = session
            .run_now("SELECT at, n, total, rolled FROM readings;")
            .unwrap()
            .pop()
            .unwrap()
        else {
            panic!("a read answers with records");
        };
        let mut held: Vec<BTreeMap<String, Value>> = records
            .into_iter()
            .map(|(_, row)| match row {
                Value::Object(fields) => fields,
                other => panic!("a row is an object, got {other:?}"),
            })
            .collect();
        held.sort_by_key(|fields| format!("{:?}", fields.get("at")));
        held
    };
    let answered = all(&mut session);
    assert_eq!(answered.len(), 2, "{answered:?}");
    let first = answered.first().unwrap_or_else(|| panic!("a rolled row"));
    assert_eq!(
        first.get("n"),
        Some(&Value::Number(tessari_types::Number::Integer(3)))
    );
    assert_eq!(
        first.get("total"),
        Some(&Value::Number(tessari_types::Number::Integer(6)))
    );
    assert_eq!(first.get("rolled"), Some(&Value::Bool(true)));
    // A re-run finds nothing raw and changes nothing.
    let again = tessari_session::roll_up_aged_series(&store).now().unwrap();
    assert_eq!(again.windows, 0, "{again:?}");
    assert_eq!(all(&mut session), answered, "a re-run changed the table");
}

#[test]
fn what_keep_raw_cannot_say_is_refused_where_it_is_written() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now("DEFINE NAMESPACE t; USE NAMESPACE t; DEFINE DATABASE d; USE DATABASE d;")
        .unwrap();
    for statement in [
        // No TIME: a window is measured on the event's own instant.
        "DEFINE SERIES a RETAIN 30d KEEP RAW FOR 7d ROLL UP BY 1h COMPUTE count(*) AS n;",
        // The marker name is the pass's own.
        "DEFINE SERIES b RETAIN 30d TIME at KEEP RAW FOR 7d ROLL UP BY 1h COMPUTE count(*) AS rolled;",
        // A window of no width.
        "DEFINE SERIES c RETAIN 30d TIME at KEEP RAW FOR 7d ROLL UP BY 0s COMPUTE count(*) AS n;",
    ] {
        let refused = session.run_now(statement);
        assert!(refused.is_err(), "{statement} ran: {refused:?}");
    }
}

/// The cluster drill for `KEEP RAW`: the pass rolls where the range is led,
/// a node that may not write refuses at the commit with the class the
/// housekeeping reads as "kept where the range is led", and a follower that
/// collects afterwards answers what the leader answered — the pass's writes
/// are ordinary committed transactions, so convergence is replication's own.
#[test]
fn keep_raw_rolls_on_the_leader_and_a_follower_converges() {
    use crate::gathered_reads::{follower_of_the_database, leader, signed_in};
    let leader = leader();
    signed_in(&leader, "root")
        .run_now(
            "USE NAMESPACE prod; USE DATABASE shop; \
             DEFINE SERIES readings RETAIN 36500d TIME at \
             KEEP RAW FOR 1s ROLL UP BY 1m COMPUTE count(*) AS n, sum(v) AS total; \
             CREATE readings = { at: datetime '2026-11-01T10:00:10Z', v: 1 }; \
             CREATE readings = { at: datetime '2026-11-01T10:00:20Z', v: 2 };",
        )
        .unwrap();
    let rolled = tessari_session::roll_up_aged_series(&leader).now().unwrap();
    assert_eq!(rolled.windows, 1, "{rolled:?}");
    // A follower of the whole database, collecting after the rollup, answers
    // the rolled row and no raw point — the pass's writes replicate like any
    // committed write's.
    let follower = follower_of_the_database(&leader);
    let mut reading = signed_in(&follower, "root");
    reading
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let Outcome::Records { records, .. } = reading
        .run_now("SELECT n, total, rolled FROM readings;")
        .unwrap()
        .pop()
        .unwrap()
    else {
        panic!("a read answers with records");
    };
    let rolled_rows: Vec<_> = records
        .iter()
        .filter(|(_, row)| format!("{row:?}").contains("rolled"))
        .collect();
    assert!(
        !rolled_rows.is_empty(),
        "the follower did not converge: {records:?}"
    );
    // The follower may not write, so the pass refuses there with the class
    // the housekeeping stays quiet about — never a partial local write.
    let refused = tessari_session::roll_up_aged_series(&follower).now();
    match refused {
        Err(tessari_session::Error::Store(
            tessari_storage::Error::LeaseSpent { .. }
            | tessari_storage::Error::NoLeadershipYet
            | tessari_storage::Error::WriteIsElsewhere { .. },
        ))
        | Ok(_) => {}
        Err(other) => panic!("an unclassified refusal: {other:?}"),
    }
}
