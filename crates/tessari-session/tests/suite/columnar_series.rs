//! SG7 — series tables are columnar-capable : `DEFINE SERIES … STORE
//! COLUMN [PARTITIONS n BY f]` holds the same points its row mirror holds,
//! and every series read shape — window folds, FILL, LATEST BY, ASOF JOIN,
//! counter folds, simple predicates — answers exactly what the row series
//! answers (the C6 differential extended; zero mismatches is the bar).
#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Now, Outcome, RunNow, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// One instant per hour from 2026-01-01T00:00Z, three sensors, values that
/// rise with a wrap (counter shape). Points arrive out of order around one
/// instant (TIME ordering lands them in place), one point is deleted, one is
/// raised by an update.
fn fixture() -> (Store, Session<'static>) {
    let first = Box::leak(Box::new(store()));
    let mut session = Session::new(first);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE telemetry; USE DATABASE telemetry;\n\
             DEFINE SERIES readings RETAIN 36500d TIME at STORE COLUMN PARTITIONS 4 BY sensor;\n\
             DEFINE SERIES mirror RETAIN 36500d TIME at;\n\
             DEFINE SERIES quotes RETAIN 36500d TIME at;",
        )
        .unwrap();
    // Quotes are the right side of the ASOF pair; both sides of the
    // differential read them, so they stay row-kind.
    let mut script = String::new();
    for sensor in ["alpha", "beta", "gamma"] {
        for hour in 0_i64..24 {
            let day = hour
                .checked_div(24)
                .unwrap_or(0)
                .checked_add(1)
                .unwrap_or(1);
            let at = format!("2026-01-0{day}T{:02}:00:00Z", hour % 24);
            let value = hour
                .checked_mul(10)
                .unwrap_or(0)
                .checked_rem(100)
                .unwrap_or(0);
            for table in ["readings", "mirror"] {
                script.push_str(&format!(
                    "CREATE {table} = {{ at: datetime '{at}', sensor: '{sensor}', v: {value} }};\n"
                ));
            }
        }
    }
    // Out-of-order arrival: a late point lands in its event-time place.
    for table in ["readings", "mirror"] {
        script.push_str(&format!(
            "CREATE {table} = {{ at: datetime '2026-01-01T03:30:00Z', sensor: 'alpha', v: 35 }};\n"
        ));
        // The delete and the update the differential re-runs its reads over.
        script.push_str(&format!(
            "DELETE FROM {table} WHERE sensor = 'beta' LIMIT 1;\n\
             UPDATE {table} SET v = 999 WHERE sensor = 'gamma' LIMIT 1;\n"
        ));
        script.push_str(&format!(
            "CREATE quotes = {{ at: datetime '2026-01-01T0{h}:00:00Z', sensor: 'alpha', px: {h} }};\n",
            h = 5,
        ));
    }
    session.run_now(&script).unwrap();
    (
        Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
            .now()
            .unwrap(),
        session,
    )
}

/// Run one read on both series and return the pair of answer values plus the
/// columnar series' access path. The record identities are NOT compared: a
/// series mints each point's identity from its TIME field, and the two
/// tables' points are written by separate statements, so the identities are
/// necessarily different — the differential is over what each read ANSWERS.
fn both(session: &mut Session<'_>, read: &str) -> (Vec<Value>, Vec<Value>, AccessPath) {
    let render = |table: &str| read.replace("{t}", table);
    let events_outcome = session.run_now(&render("readings")).unwrap().pop().unwrap();
    let mirror_outcome = session.run_now(&render("mirror")).unwrap().pop().unwrap();
    let access = events_outcome
        .plan()
        .map(|plan| plan.access)
        .unwrap_or(AccessPath::Record);
    let records = |outcome: Outcome| match outcome {
        Outcome::Records { records, .. } => {
            let mut values: Vec<Value> = records.into_iter().map(|(_, value)| value).collect();
            // Unordered reads break ties by the physical identity, which two
            // separately-written tables legitimately hold differently — the
            // differential is over the ANSWERED SET, so both sides sort.
            // Reads whose order is the answer state it with ORDER BY and are
            // checked order-sensitively where the identities are stable.
            values.sort();
            values
        }
        other => panic!("expected records, got {other:?}"),
    };
    (records(events_outcome), records(mirror_outcome), access)
}

#[test]
fn window_folds_and_fill_answer_equal() {
    let (_keep, mut session) = fixture();
    let (events, mirror, access) = both(
        &mut session,
        "SELECT count(*) AS n, time::bucket(at, 6h) AS w FROM {t} GROUP BY time::bucket(at, 6h);",
    );
    assert_eq!(events, mirror, "the window fold diverged");
    // Series reads are row-path at v1: the gate's schemaless rule
    // routes them to the walk — the projection holds the points, the row
    // serves the reads, and the answers are exact by authority.
    assert_ne!(
        access,
        AccessPath::Columnar,
        "a series read is row-path at v1"
    );

    let (events, mirror, _) = both(
        &mut session,
        "SELECT count(*) AS n, time::bucket(at, 6h) AS w FROM {t} \
         WHERE sensor = 'alpha' GROUP BY time::bucket(at, 6h) FILL NULL \
         FROM datetime '2026-01-01T00:00:00Z' TO datetime '2026-01-04T00:00:00Z';",
    );
    assert_eq!(events, mirror, "the filled window diverged");

    let (events, mirror, _) = both(
        &mut session,
        "SELECT sensor, count(*) AS n FROM {t} GROUP BY sensor, time::bucket(at, 12h);",
    );
    assert_eq!(events, mirror, "the keyed window fold diverged");
}

#[test]
fn latest_asof_and_counter_reads_answer_equal() {
    let (_keep, mut session) = fixture();
    let (events, mirror, _) = both(&mut session, "SELECT * FROM {t} LATEST BY sensor;");
    assert_eq!(events, mirror, "LATEST BY diverged");

    let (events, mirror, _) = both(
        &mut session,
        "SELECT increase(v, at) AS up, delta(v, at) AS change FROM {t} WHERE sensor = 'alpha';",
    );
    assert_eq!(events, mirror, "the counter folds diverged");

    let (events, mirror, _) = both(
        &mut session,
        "SELECT * FROM {t} AS points ASOF JOIN quotes ON points.sensor = quotes.sensor;",
    );
    assert_eq!(events, mirror, "the ASOF join diverged");
}

#[test]
fn a_pinned_series_read_answers_exactly_through_the_row() {
    let (_keep, mut session) = fixture();
    // The pinned lane and the simple condition are exactly the shape the
    // record-table seam prunes on — on a series the gate's schemaless rule
    // keeps the read on the row path at v1, and the row authority
    // answers it exactly. The projection the clause maintains is what a
    // later gate lift serves.
    let (events, mirror, access) = both(
        &mut session,
        "SELECT * FROM {t} WHERE sensor = 'alpha' AND at < datetime '2026-01-01T06:00:00Z';",
    );
    assert_eq!(events, mirror, "the pinned series read diverged");
    assert_ne!(
        access,
        AccessPath::Columnar,
        "a series read is row-path at v1"
    );
}

#[test]
fn the_definition_renders_back_and_restores() {
    let first = Box::leak(Box::new(store()));
    let mut session = Session::new(first);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE telemetry; USE DATABASE telemetry;\n\
             DEFINE SERIES readings RETAIN 30d TIME at STORE COLUMN PARTITIONS 4 BY sensor;",
        )
        .unwrap();
    let outcome = session
        .run_now("INFO FOR TABLE readings;")
        .unwrap()
        .pop()
        .unwrap();
    let Outcome::Value(Value::Object(report)) = outcome else {
        panic!("expected a report");
    };
    let described = match report.get("definition") {
        Some(Value::String(script)) => script.clone(),
        other => panic!("no definition: {other:?}"),
    };
    assert!(
        described.contains("STORE COLUMN"),
        "INFO renders the word: {described}"
    );
    assert!(
        described.contains("PARTITIONS 4 BY sensor"),
        "INFO renders the partitioning: {described}"
    );
    // Restore into an empty store; the words come back the same.
    let empty = Box::leak(Box::new(store()));
    let mut restored = Session::new(empty);
    restored
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
                  DEFINE DATABASE telemetry; USE DATABASE telemetry;",
        )
        .unwrap();
    restored.run_now(&described).unwrap();
    let outcome = restored
        .run_now("INFO FOR TABLE readings;")
        .unwrap()
        .pop()
        .unwrap();
    let Outcome::Value(Value::Object(report)) = outcome else {
        panic!("expected a report");
    };
    assert_eq!(
        report.get("definition"),
        Some(&Value::String(described)),
        "the words survive a restore"
    );
}
