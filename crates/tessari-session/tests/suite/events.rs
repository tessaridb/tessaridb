//! `DEFINE EVENT` — logic that runs after a record write, in the writer's
//! transaction, as the writer (ADR-0110).
//!
//! The property every test here leans on is that the event's effects and the
//! write are ONE commit: a refusal in the body refuses the write, a cancelled
//! transaction leaves neither, and the work that must happen after the commit
//! goes through a topic appended in the same commit.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    Session::new(&store)
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; \
             DEFINE COLLECTION orders; DEFINE COLLECTION log;",
        )
        .unwrap();
    store
}

fn inside(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// The records a read answered, as objects.
fn rows(session: &mut Session<'_>, read: &str) -> Vec<BTreeMap<String, Value>> {
    let outcomes = session.run(read).unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("{read}: {:?}", outcomes.last());
    };
    records
        .iter()
        .map(|(_, value)| match value {
            Value::Object(fields) => fields.clone(),
            other => panic!("{read}: not an object: {other:?}"),
        })
        .collect()
}

fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(Number::Integer(held))) => *held,
        other => panic!("not an integer: {other:?}"),
    }
}

fn text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(held)) => held.clone(),
        other => panic!("not a string: {other:?}"),
    }
}

/// `(event, was, now)` for every log row, sorted.
fn logged(session: &mut Session<'_>) -> Vec<(String, i64, i64)> {
    let mut seen: Vec<_> = rows(session, "SELECT * FROM log;")
        .iter()
        .map(|row| {
            (
                text(row.get("event")),
                int(row.get("was")),
                int(row.get("now")),
            )
        })
        .collect();
    seen.sort();
    seen
}

/// The innermost refusal under the events a refusal passed through.
fn innermost(refused: &Error) -> &Error {
    let mut cause = refused;
    while let Error::EventFailed { cause: inner, .. } = cause {
        cause = inner;
    }
    cause
}

const AUDIT: &str = "DEFINE EVENT audit ON orders THEN \
     CREATE log = { event: $event, key: $id, was: $before.v ?? -1, now: $after.v ?? -1 };";

#[test]
fn an_event_sees_each_write_as_it_was_and_as_it_is() {
    let store = store();
    let mut session = inside(&store);
    session.run(AUDIT).unwrap();
    session
        .run("CREATE orders:1 = { v: 3 }; UPDATE orders:1 SET v = 5; DELETE orders:1;")
        .unwrap();
    assert_eq!(
        logged(&mut session),
        vec![
            ("CREATE".to_owned(), -1, 3),
            ("DELETE".to_owned(), 5, -1),
            ("UPDATE".to_owned(), 3, 5),
        ]
    );
    // `$id` is the record's identity, the value written after `orders:`.
    let keyed = rows(&mut session, "SELECT * FROM log WHERE key = 1;");
    assert_eq!(keyed.len(), 3);
    // A record that was not there was not deleted: nothing fires.
    session.run("DELETE orders:99;").unwrap();
    assert_eq!(logged(&mut session).len(), 3);
}

#[test]
fn for_and_when_choose_which_writes_run_the_body() {
    let store = store();
    let mut session = inside(&store);
    session
        .run(
            "DEFINE EVENT big ON orders FOR UPDATE WHEN $after.v > 10 THEN \
             CREATE log = { event: $event, was: $before.v, now: $after.v };",
        )
        .unwrap();
    session
        .run(
            "CREATE orders:1 = { v: 50 }; UPDATE orders:1 SET v = 4; \
             UPDATE orders:1 SET v = 40; DELETE orders:1;",
        )
        .unwrap();
    assert_eq!(logged(&mut session), vec![("UPDATE".to_owned(), 4, 40)]);
}

#[test]
fn a_body_of_several_statements_runs_in_order_and_may_bind() {
    let store = store();
    let mut session = inside(&store);
    session
        .run(
            "DEFINE EVENT twice ON orders FOR CREATE THEN { \
                 LET $doubled = $after.v * 2; \
                 CREATE log = { event: 'first', was: $after.v, now: $doubled }; \
                 CREATE log = { event: 'second', was: $doubled, now: $doubled + 1 }; \
             };",
        )
        .unwrap();
    session.run("CREATE orders:1 = { v: 7 };").unwrap();
    assert_eq!(
        logged(&mut session),
        vec![("first".to_owned(), 7, 14), ("second".to_owned(), 14, 15)]
    );
}

#[test]
fn a_refusal_in_the_body_refuses_the_write_and_nothing_lands() {
    let store = store();
    let mut session = inside(&store);
    session
        .run(
            "DEFINE EVENT audit ON orders THEN CREATE log = { event: $event, was: 0, now: 0 }; \
             DEFINE EVENT positive ON orders WHEN $after.v < 0 THEN THROW 'an order is never negative';",
        )
        .unwrap();
    let refused = session.run("CREATE orders:2 = { v: -1 };").unwrap_err();
    let Error::EventFailed {
        event,
        table,
        cause,
    } = &refused
    else {
        panic!("{refused:?}");
    };
    assert_eq!((event.as_str(), table.as_str()), ("positive", "orders"));
    assert!(matches!(**cause, Error::Thrown { .. }), "{cause:?}");
    assert!(refused.to_string().contains("never negative"), "{refused}");
    // Neither the record nor the other event's row landed: one commit.
    assert!(rows(&mut session, "SELECT * FROM orders;").is_empty());
    assert!(logged(&mut session).is_empty());
    // Inside a transaction the statement fails and the transaction holds
    // nothing of it.
    let inside_one =
        session.run("BEGIN; CREATE orders:3 = { v: 1 }; CREATE orders:4 = { v: -2 }; COMMIT;");
    assert!(
        matches!(inside_one, Err(Error::EventFailed { .. })),
        "{inside_one:?}"
    );
    assert!(rows(&mut session, "SELECT * FROM orders;").is_empty());
}

#[test]
fn an_event_that_writes_its_own_table_is_bounded_and_named() {
    let store = store();
    let mut session = inside(&store);
    session
        .run("DEFINE EVENT again ON orders FOR UPDATE THEN UPDATE orders:$id SET v = $after.v + 1;")
        .unwrap();
    session.run("CREATE orders:1 = { v: 0 };").unwrap();
    let refused = session.run("UPDATE orders:1 SET v = 1;").unwrap_err();
    let Error::EventDepth { event, limit } = innermost(&refused) else {
        panic!("{refused:?}");
    };
    assert_eq!(event, "again");
    assert_eq!(*limit, tessari_constants::EVENT_DEPTH_LIMIT);
    assert_eq!(
        int(rows(&mut session, "SELECT * FROM orders:1;")[0].get("v")),
        0
    );
    // A guard that excludes the event's own change ends the chain.
    session
        .run(
            "DROP EVENT again ON orders; \
             DEFINE EVENT capped ON orders FOR UPDATE WHEN $after.v < 5 THEN UPDATE orders:$id SET v = $after.v + 1;",
        )
        .unwrap();
    session.run("UPDATE orders:1 SET v = 1;").unwrap();
    assert_eq!(
        int(rows(&mut session, "SELECT * FROM orders:1;")[0].get("v")),
        5
    );
}

#[test]
fn a_cycle_between_two_tables_is_refused_at_the_limit() {
    let store = store();
    let mut session = inside(&store);
    session
        .run(
            "DEFINE COLLECTION mirror; \
             DEFINE EVENT out ON orders THEN UPSERT mirror:1 = { v: $after.v ?? 0 }; \
             DEFINE EVENT back ON mirror THEN UPSERT orders:1 = { v: $after.v ?? 0 };",
        )
        .unwrap();
    let refused = session.run("CREATE orders:1 = { v: 1 };").unwrap_err();
    assert!(
        matches!(innermost(&refused), Error::EventDepth { .. }),
        "{refused:?}"
    );
    assert!(rows(&mut session, "SELECT * FROM orders;").is_empty());
    assert!(rows(&mut session, "SELECT * FROM mirror;").is_empty());
}

#[test]
fn the_body_runs_with_the_writers_authority() {
    let store = store();
    let mut setup = inside(&store);
    setup
        .run(&format!(
            "DEFINE EVENT audit ON orders THEN CREATE log = {{ event: $event, was: 0, now: 0 }}; \
             DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
        ))
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(&format!(
        "DEFINE USER clerk ON prod.shop ROLE editor PASSWORD '{PASSWORD}'; \
         USE NAMESPACE prod; USE DATABASE shop; GRANT read, write ON orders TO clerk;"
    ))
    .unwrap();
    let mut clerk = Session::new(&store);
    clerk.sign_in("clerk", PASSWORD).unwrap();
    clerk.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    // The clerk may write orders and not the log, so the audited write is
    // refused — the audit is part of the write.
    let refused = clerk.run("CREATE orders:1 = { v: 1 };").unwrap_err();
    assert!(matches!(refused, Error::EventFailed { .. }), "{refused:?}");
    root.run("GRANT read, write ON log TO clerk;").unwrap();
    clerk.run("CREATE orders:1 = { v: 1 };").unwrap();
    assert_eq!(logged(&mut root).len(), 1);
}

#[test]
fn work_after_the_commit_is_a_topic_appended_with_the_write() {
    let store = store();
    let mut session = inside(&store);
    session
        .run(
            "DEFINE TOPIC order_events; \
             DEFINE EVENT outbox ON orders FOR CREATE THEN \
             CREATE order_events = { order: $id, v: $after.v };",
        )
        .unwrap();
    // A cancelled write leaves neither the record nor the message.
    session
        .run("BEGIN; CREATE orders:1 = { v: 1 }; CANCEL;")
        .unwrap();
    assert!(rows(&mut session, "READ FROM order_events;").is_empty());
    session.run("CREATE orders:2 = { v: 2 };").unwrap();
    let first = rows(&mut session, "READ FROM order_events FOR CONSUMER 'mail';");
    assert_eq!(first.len(), 1);
    let Some(Value::Object(message)) = first[0].get("value") else {
        panic!("{first:?}");
    };
    assert_eq!(int(message.get("v")), 2);
    // The identity, and the record it names reads back through it.
    assert_eq!(int(message.get("order")), 2);
    // Read once: the consumer's position moved past it.
    assert!(rows(&mut session, "READ FROM order_events FOR CONSUMER 'mail';").is_empty());
}

#[test]
fn info_reports_an_event_as_its_statement_and_drop_stops_it() {
    let store = store();
    let mut session = inside(&store);
    session.run(AUDIT).unwrap();
    // Defining it again is refused; `IF NOT EXISTS` accepts.
    let again = session.run(AUDIT).unwrap_err();
    assert!(matches!(again, Error::EventExists { .. }), "{again:?}");
    session
        .run(&AUDIT.replace("DEFINE EVENT audit", "DEFINE EVENT IF NOT EXISTS audit"))
        .unwrap();
    let outcomes = session.run("INFO FOR TABLE orders;").unwrap();
    let rendered = format!("{outcomes:?}");
    assert!(
        rendered.contains("DEFINE EVENT audit ON orders THEN CREATE log"),
        "{rendered}"
    );
    session.run("DROP EVENT audit ON orders;").unwrap();
    session.run("CREATE orders:1 = { v: 1 };").unwrap();
    assert!(logged(&mut session).is_empty());
    let missing = session.run("DROP EVENT audit ON orders;").unwrap_err();
    assert!(
        matches!(
            missing,
            Error::Unknown {
                entity: "event",
                ..
            }
        ),
        "{missing:?}"
    );
}

#[test]
fn a_body_that_could_not_run_is_refused_where_it_is_written() {
    let store = store();
    let mut session = inside(&store);
    for body in [
        "BEGIN",
        "DEFINE COLLECTION other",
        "USE DATABASE other",
        "SELECT * FROM log",
        "GRANT read ON log TO nobody",
    ] {
        let refused = session
            .run(&format!("DEFINE EVENT bad ON orders THEN {body};"))
            .unwrap_err();
        assert!(
            matches!(refused, Error::Script(tessari_ql::Error::EventBody { .. })),
            "{body}: {refused:?}"
        );
    }
    // A parameter the event does not bind is refused at DEFINE, not at the
    // first write.
    let refused = session
        .run("DEFINE EVENT bad ON orders THEN CREATE log = { v: $aftr.v };")
        .unwrap_err();
    assert!(
        matches!(
            refused,
            Error::Script(tessari_ql::Error::UnboundParameter { .. })
        ),
        "{refused:?}"
    );
    // Nothing was defined.
    session.run("CREATE orders:1 = { v: 1 };").unwrap();
    assert!(logged(&mut session).is_empty());
}

#[test]
fn only_tables_collections_and_edges_carry_events() {
    let store = store();
    let mut session = inside(&store);
    session
        .run(
            "UNSEAL VAULT WITH 'an operator passphrase'; DEFINE VAULT secrets; DEFINE TOPIC feed; DEFINE QUEUE jobs TIMEOUT 30s ATTEMPTS 3; \
             DEFINE VIEW recent AS SELECT * FROM orders;",
        )
        .unwrap();
    for table in ["secrets", "feed", "jobs", "recent"] {
        let refused = session
            .run(&format!(
                "DEFINE EVENT e ON {table} THEN CREATE log = {{ v: 1 }};"
            ))
            .unwrap_err();
        let Error::EventOnKind { table: named, .. } = &refused else {
            panic!("{table}: {refused:?}");
        };
        assert_eq!(named, table);
    }
    let refused = session
        .run("DEFINE EVENT e ON nowhere THEN CREATE log = { v: 1 };")
        .unwrap_err();
    assert!(
        matches!(
            refused,
            Error::Unknown {
                entity: "table",
                ..
            }
        ),
        "{refused:?}"
    );
}

/// A multiplicative generator, so a run is a function of its seed.
struct Rolls(u64);

impl Rolls {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33).checked_rem(bound).unwrap_or(0)
    }
}

/// Random upserts and deletes, logged by the event, equal a model of what each
/// write replaced — and a control arm with the event dropped logs nothing more.
#[test]
fn the_log_an_event_keeps_equals_a_model_of_every_write() {
    for seed in 0..8_u64 {
        let store = store();
        let mut session = inside(&store);
        session.run(AUDIT).unwrap();
        let mut rolls = Rolls(seed);
        let mut model: BTreeMap<u64, i64> = BTreeMap::new();
        let mut expected = Vec::new();
        for step in 0..120_i64 {
            let key = rolls.below(12);
            if rolls.below(4) == 0 {
                session.run(&format!("DELETE orders:{key};")).unwrap();
                if let Some(was) = model.remove(&key) {
                    expected.push(("DELETE".to_owned(), was, -1));
                }
            } else {
                session
                    .run(&format!("UPSERT orders:{key} = {{ v: {step} }};"))
                    .unwrap();
                match model.insert(key, step) {
                    Some(was) => expected.push(("UPDATE".to_owned(), was, step)),
                    None => expected.push(("CREATE".to_owned(), -1, step)),
                }
            }
        }
        expected.sort();
        assert!(expected.len() > 80, "seed {seed}: {}", expected.len());
        assert_eq!(logged(&mut session), expected, "seed {seed}");
        // Control arm: the same kind of write with the event gone logs nothing.
        session
            .run("DROP EVENT audit ON orders; UPSERT orders:1 = { v: 1 };")
            .unwrap();
        assert_eq!(logged(&mut session).len(), expected.len(), "seed {seed}");
    }
}

#[test]
fn an_edge_table_runs_its_events_as_edges_come_and_go() {
    let store = store();
    let mut session = inside(&store);
    session
        .run(
            "DEFINE COLLECTION users; DEFINE TABLE follows EDGE; \
             CREATE users:1 = { v: 1 }; CREATE users:2 = { v: 2 }; \
             DEFINE EVENT seen ON follows THEN CREATE log = { event: $event, was: 0, now: 0 };",
        )
        .unwrap();
    session
        .run("RELATE users:1->follows->users:2; DELETE users:1->follows->users:2;")
        .unwrap();
    assert_eq!(
        logged(&mut session),
        vec![("CREATE".to_owned(), 0, 0), ("DELETE".to_owned(), 0, 0)]
    );
}

/// A script backup carries the event and replays the records without running
/// it, so the restored store holds each effect once — and then runs it.
#[test]
fn a_script_backup_restores_an_event_without_running_it_twice() {
    let store = store();
    let mut session = inside(&store);
    session.run(AUDIT).unwrap();
    session.run("CREATE orders:1 = { v: 1 };").unwrap();
    let Some(Outcome::Value(Value::String(script))) = session.run("BACKUP SCRIPT;").unwrap().pop()
    else {
        panic!("BACKUP SCRIPT answered no text");
    };
    assert!(script.contains("DEFINE EVENT audit ON orders"), "{script}");
    let restored = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    Session::new(&restored).run(&script).unwrap();
    let mut session = inside(&restored);
    assert_eq!(logged(&mut session), vec![("CREATE".to_owned(), -1, 1)]);
    session.run("UPDATE orders:1 SET v = 2;").unwrap();
    assert_eq!(
        logged(&mut session),
        vec![("CREATE".to_owned(), -1, 1), ("UPDATE".to_owned(), 1, 2)]
    );
}

/// A body sees the record as the writer may read it: a writer who may write the
/// table and not read it gives the body no `$after`, so the event cannot copy
/// what they cannot read somewhere they can (ADR-0110 D6).
#[test]
fn a_body_sees_only_the_fields_the_writer_may_read() {
    let store = store();
    let mut setup = inside(&store);
    setup
        .run(&format!(
            "DEFINE EVENT copy ON orders THEN \
             CREATE log = {{ event: $event, was: 0, now: 0, leaked: $after.secret ?? 'hidden' }}; \
             DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
        ))
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(&format!(
        "DEFINE USER clerk ON prod.shop ROLE editor PASSWORD '{PASSWORD}'; \
         USE NAMESPACE prod; USE DATABASE shop; \
         GRANT write ON orders TO clerk; GRANT read, write ON log TO clerk;"
    ))
    .unwrap();
    let mut clerk = Session::new(&store);
    clerk.sign_in("clerk", PASSWORD).unwrap();
    clerk.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    clerk
        .run("CREATE orders:1 = { v: 1, secret: 42 };")
        .unwrap();
    let copied = rows(&mut root, "SELECT * FROM log;");
    assert_eq!(copied.len(), 1);
    assert_eq!(text(copied[0].get("leaked")), "hidden");
    // Control: the owner reads every field, so the same event copies it.
    root.run("CREATE orders:2 = { v: 2, secret: 43 };").unwrap();
    let copied = rows(&mut root, "SELECT * FROM log WHERE leaked = 43;");
    assert_eq!(copied.len(), 1);
}
