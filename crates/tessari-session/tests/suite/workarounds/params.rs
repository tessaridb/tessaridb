//! `DEFINE PARAM` — a database's named values, read wherever a parameter is
//! (ADR-0124 D2).

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::Store;
use tessari_types::{Duration, Number, Value};

use super::{inside, refused, rows, run, store, value};

fn int(n: i64) -> Value {
    Value::Number(Number::Integer(n))
}

fn minutes(n: i64) -> Value {
    Value::Duration(Duration::from_seconds(n.saturating_mul(60)))
}

#[test]
fn a_param_is_read_in_the_script_that_defines_it_and_in_every_later_one() {
    let store = store();
    let mut session = inside(&store);
    assert_eq!(
        value(
            &mut session,
            "DEFINE PARAM $grace VALUE 10m; RETURN $grace;"
        ),
        minutes(10)
    );
    let mut later = inside(&store);
    assert_eq!(value(&mut later, "RETURN $grace;"), minutes(10));
    assert_eq!(
        value(&mut later, "RETURN time::now() - $grace < time::now();"),
        Value::Bool(true)
    );
}

#[test]
fn the_callers_value_and_a_let_are_closer_than_the_databases() {
    let store = store();
    let mut session = inside(&store);
    run(&mut session, "DEFINE PARAM $grace VALUE 10m;");
    let bound = session
        .run_with(
            "RETURN $grace;",
            &BTreeMap::from([("grace".to_owned(), int(1))]),
        )
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(bound, tessari_session::Outcome::Value(int(1)));
    assert_eq!(
        value(&mut session, "LET $grace = 2; RETURN $grace;"),
        int(2)
    );
}

#[test]
fn a_param_is_kept_replaced_or_refused_as_the_definition_says() {
    let store = store();
    let mut session = inside(&store);
    run(&mut session, "DEFINE PARAM $grace VALUE 10m;");
    let error = refused(&mut session, "DEFINE PARAM $grace VALUE 20m;");
    assert!(matches!(error, Error::ParamExists { .. }), "{error:?}");
    run(&mut session, "DEFINE PARAM IF NOT EXISTS $grace VALUE 30m;");
    assert_eq!(value(&mut session, "RETURN $grace;"), minutes(10), "kept");
    run(&mut session, "DEFINE PARAM OR REPLACE $grace VALUE 40m;");
    assert_eq!(
        value(&mut session, "RETURN $grace;"),
        minutes(40),
        "replaced"
    );
    let info = value(&mut session, "INFO FOR DATABASE;");
    let Value::Object(fields) = info else {
        panic!("{info:?}")
    };
    assert_eq!(
        fields.get("params"),
        Some(&Value::Object(BTreeMap::from([(
            "grace".to_owned(),
            minutes(40)
        )])))
    );
}

#[test]
fn an_event_reads_the_value_the_param_holds_when_it_runs() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE PARAM $grace VALUE 10m; DEFINE COLLECTION objects; DEFINE COLLECTION reclaims; \
         DEFINE EVENT queue_it ON objects THEN CREATE reclaims = { grace: $grace };",
    );
    run(&mut session, "CREATE objects:1 = {};");
    run(&mut session, "DEFINE PARAM OR REPLACE $grace VALUE 1m;");
    run(&mut session, "CREATE objects:2 = {};");
    let mut seen: Vec<Value> = rows(&mut session, "SELECT grace FROM reclaims;")
        .into_iter()
        .filter_map(|row| row.get("grace").cloned())
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![minutes(1), minutes(10)],
        "no redefinition of the event"
    );
}

#[test]
fn an_event_naming_a_param_the_database_lacks_is_refused_where_it_is_written() {
    let store = store();
    let mut session = inside(&store);
    run(&mut session, "DEFINE COLLECTION objects;");
    let error = refused(
        &mut session,
        "DEFINE EVENT e ON objects THEN CREATE objects = { g: $nowhere };",
    );
    assert!(error.to_string().contains("$nowhere"), "{error}");
}

#[test]
fn a_view_reads_a_param() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE PARAM $floor VALUE 2; DEFINE COLLECTION n; \
         CREATE n:1 = { v: 1 }; CREATE n:2 = { v: 2 }; CREATE n:3 = { v: 3 }; \
         DEFINE VIEW big AS SELECT * FROM n WHERE v >= $floor;",
    );
    assert_eq!(rows(&mut session, "SELECT * FROM big;").len(), 2);
}

#[test]
fn a_param_belongs_to_its_database() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE PARAM $grace VALUE 10m; DEFINE DATABASE other;",
    );
    let error = refused(&mut session, "USE DATABASE other; RETURN $grace;");
    assert!(error.to_string().contains("$grace"), "{error}");
    // Per statement: the `USE` back makes the same name resolve again.
    assert_eq!(
        value(
            &mut session,
            "USE DATABASE other; USE DATABASE app; RETURN $grace;"
        ),
        minutes(10)
    );
}

#[test]
fn a_dropped_param_is_gone_and_if_exists_skips_an_absent_one() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE PARAM $grace VALUE 10m; DROP PARAM $grace;",
    );
    assert!(
        refused(&mut session, "RETURN $grace;")
            .to_string()
            .contains("$grace")
    );
    run(&mut session, "DROP PARAM IF EXISTS $grace;");
    // In one script too: what runs after the drop does not see the value.
    let error = refused(
        &mut session,
        "DEFINE PARAM $g VALUE 1; DROP PARAM $g; RETURN $g;",
    );
    assert!(error.to_string().contains("$g"), "{error}");
    run(&mut session, "DEFINE PARAM $h VALUE 1;");
    let error = refused(&mut session, "DROP PARAM $h; RETURN $h;");
    assert!(error.to_string().contains("$h"), "{error}");
    let error = refused(&mut session, "DROP PARAM $grace;");
    assert!(matches!(error, Error::Unknown { .. }), "{error:?}");
}

#[test]
fn a_param_travels_in_a_script_backup_and_a_snapshot() {
    let store = store();
    let mut session = inside(&store);
    run(&mut session, "DEFINE PARAM $grace VALUE 10m;");
    let Value::String(script) = value(&mut session, "BACKUP SCRIPT;") else {
        panic!("a script is text")
    };
    let from_script = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    Session::new(&from_script)
        .run(&script)
        .unwrap_or_else(|why| panic!("{why}\n{script}"));
    assert_eq!(
        value(&mut inside(&from_script), "RETURN $grace;"),
        minutes(10)
    );

    let Value::Bytes(snapshot) = value(&mut session, "BACKUP STATE;") else {
        panic!("a snapshot is bytes")
    };
    let from_snapshot = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    tessari_backup::read_state(&from_snapshot, || {
        Ok(std::io::Cursor::new(snapshot.clone()))
    })
    .unwrap();
    assert_eq!(
        value(&mut inside(&from_snapshot), "RETURN $grace;"),
        minutes(10)
    );
}
