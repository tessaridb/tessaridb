//! The source side of a cross-cluster link: what the driver ships, and what
//! the confirmation bounds.
//!
//! These tests hold the contract [`tessari_storage::serve_once`] serves a
//! declared [`tessari_storage::ReplicationDefinition`] with: only the linked
//! database's records cross (the filter is on this side — a confidentiality
//! model in which the party being restricted applies the restriction is the
//! one this store refuses everywhere else), the envelope carries the origin
//! and the positions the far side needs to place each batch, and at-least-once
//! is bounded at the confirmation point — a batch the sink has not confirmed
//! is read again, byte for the same records.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Now;
use tessari_session::RunNow;
use tessari_session::Session;
use tessari_storage::{
    Catalog, ChannelSink, Envelope, Error, Links, Reach, ReplicationDefinition, Result, Sink,
    Store, intake, serve_links, serve_once,
};
use tessari_types::Value;
use tessari_types::{NamespaceId, Sequence};

fn backend() -> Arc<dyn KvBackend> {
    Arc::new(MemoryBackend::new())
}

/// A store with a database pair and a link declared, ready to serve.
fn store_with_a_link() -> Store {
    let store = Store::open(backend()).now().unwrap();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; \
             USE NAMESPACE prod; \
             DEFINE DATABASE shop; \
             DEFINE DATABASE other; \
             USE DATABASE shop; DEFINE TABLE orders SCHEMALESS; \
             USE DATABASE other; DEFINE TABLE orders SCHEMALESS; \
             USE DATABASE shop; \
             DEFINE REPLICATION east TO 'cluster-b:9080' FOR DATABASE prod.shop;",
        )
        .unwrap();
    store
}

fn definition(store: &Store, name: &str) -> ReplicationDefinition {
    let mut transaction = store.begin().now().unwrap();
    Catalog::new(&mut transaction)
        .replications()
        .now()
        .unwrap()
        .into_iter()
        .find(|held| held.name == name)
        .unwrap_or_else(|| panic!("no replication `{name}`"))
}

/// The database a reach names, when it names one.
fn reach_database(reach: Reach) -> Option<(NamespaceId, tessari_types::DatabaseId)> {
    match reach {
        Reach::Database(namespace, database) => Some((namespace, database)),
        _ => None,
    }
}

/// Every (namespace, database) a data mutation in the envelope belongs to.
/// Catalog mutations (the system tenancy) are schema-class and cross by the
/// carried rules — a database's name record travels at its parent's reach,
/// the way `carried_to` classifies it — so the data assertion looks at
/// ordinary tenancies only.
fn data_tenancies(envelope: &Envelope) -> Vec<(NamespaceId, tessari_types::DatabaseId)> {
    let mut held = Vec::new();
    for (_, record) in &envelope.records {
        for mutation in record.mutations() {
            if mutation.namespace != NamespaceId::new(0) {
                held.push((mutation.namespace, mutation.database));
            }
        }
    }
    held
}

#[test]
fn a_link_ships_only_its_database() {
    let store = store_with_a_link();
    let mut session = Session::new(&store);
    session
        .run_now(
            "USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' }; \
             USE NAMESPACE prod DATABASE other; CREATE orders:2 = { item: 'pen' };",
        )
        .unwrap();
    let definition = definition(&store, "east");
    let registry = Links::new();
    let mut sink = ChannelSink::new();
    serve_once(&store, &definition, &registry, &mut sink)
        .now()
        .unwrap();
    let shipped = sink.drain();
    assert!(
        !shipped.is_empty(),
        "the link shipped nothing though its database was written"
    );
    for (_, envelope) in &shipped {
        assert_eq!(
            envelope.origin,
            store.node_identity().now().unwrap().id,
            "the envelope names the cluster that wrote what it carries"
        );
        let expected = reach_database(definition.database)
            .unwrap_or_else(|| panic!("a database reach names a database"));
        for tenancy in data_tenancies(envelope) {
            assert_eq!(
                tenancy, expected,
                "no data outside the linked database crossed: {envelope:?}"
            );
        }
    }
}

#[test]
fn a_confirmed_batch_is_not_shipped_again() {
    let store = store_with_a_link();
    let mut session = Session::new(&store);
    session
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let definition = definition(&store, "east");
    let registry = Links::new();
    let mut sink = ChannelSink::new();
    serve_once(&store, &definition, &registry, &mut sink)
        .now()
        .unwrap();
    let first = sink.drain();
    assert!(!first.is_empty());
    // A second pass with nothing new written ships nothing: the confirmed
    // position is the boundary, and reading begins one past it.
    serve_once(&store, &definition, &registry, &mut sink)
        .now()
        .unwrap();
    assert!(
        sink.drain().is_empty(),
        "confirmed batches ship again: at-least-once is bounded at the \
         confirmation point, not at the read"
    );
}

#[test]
fn an_unconfirmed_batch_is_read_again_verbatim() {
    // The boundary, from the other side: a sink that has not confirmed keeps
    // the batch on this side, and the re-read carries the same records — what
    // the far side eventually applies is identical, which is what makes the
    // re-application idempotent rather than merely duplicate.
    let store = store_with_a_link();
    let mut session = Session::new(&store);
    session
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let definition = definition(&store, "east");
    let registry = Links::new();

    struct Refusing {
        seen: Option<Envelope>,
    }
    impl Sink for Refusing {
        fn ship(
            &mut self,
            _link: &str,
            envelope: Envelope,
        ) -> impl std::future::Future<Output = Result<Sequence>> + Send {
            self.seen = Some(envelope);
            std::future::ready(Err(Error::LinkRefused {
                reason: "the far side is not answering".to_owned(),
            }))
        }
    }
    let mut refusing = Refusing { seen: None };
    assert!(
        serve_once(&store, &definition, &registry, &mut refusing)
            .now()
            .is_err(),
        "an unconfirmed batch is an error the caller paces, not a silent drop"
    );
    let first = refusing.seen.expect("the batch reached the sink");
    // Nothing advanced: the registry still reads from the log's start, and the
    // re-read carries the same batch — what the far side eventually applies is
    // identical, which is what makes re-application idempotent.
    let mut sink = ChannelSink::new();
    serve_once(&store, &definition, &registry, &mut sink)
        .now()
        .unwrap();
    let second = sink.drain();
    assert!(
        second
            .iter()
            .any(|(_, envelope)| envelope.records == first.records && !envelope.records.is_empty()),
        "the unconfirmed batch was read again verbatim: {second:?}"
    );
}

#[test]
fn a_fresh_link_ships_schema_but_no_data() {
    // A declared link's own log already holds the database's definitions, and
    // those cross — schema travels down the containment order by the carried
    // rules, a database's name record with its parent's reach. What must not
    // cross is DATA: no ordinary tenancy's record ships before anything wrote
    // to the linked database, and the link's own row (StoreOnly — this
    // cluster's topology) never ships at all.
    let store = store_with_a_link();
    let definition = definition(&store, "east");
    let registry = Links::new();
    let mut sink = ChannelSink::new();
    serve_once(&store, &definition, &registry, &mut sink)
        .now()
        .unwrap();
    let shipped = sink.drain();
    assert!(
        !shipped.is_empty(),
        "the link's own declaration gave the log something to ship"
    );
    for (_, envelope) in &shipped {
        assert!(
            data_tenancies(envelope).is_empty(),
            "no data crossed before anything was written: {envelope:?}"
        );
    }
}

#[test]
fn drop_forgets_the_confirmed_positions() {
    let store = store_with_a_link();
    let mut session = Session::new(&store);
    session
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let definition = definition(&store, "east");
    let registry = Links::new();
    let mut sink = ChannelSink::new();
    serve_once(&store, &definition, &registry, &mut sink)
        .now()
        .unwrap();
    sink.drain();
    registry.drop_link("east");
    // A re-declared link of the same name starts from the logs' starts again:
    // the positions died with the link, which is what dropping one means.
    let mut sink = ChannelSink::new();
    serve_once(&store, &definition, &registry, &mut sink)
        .now()
        .unwrap();
    assert!(
        !sink.drain().is_empty(),
        "dropping the link forgot the confirmation boundary"
    );
}

#[test]
fn the_served_group_reports_what_the_far_side_confirmed() {
    // The lag surface: positions, both sides able to read them. The served
    // group is this-process state — empty after a restart, like a consumer's
    // running state — so the report says what this process has shipped and
    // the far side has confirmed, nothing older.
    let store = store_with_a_link();
    let mut session = Session::new(&store);
    session
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let definition = definition(&store, "east");
    let registry = store.links();
    let mut sink = ChannelSink::new();
    serve_once(&store, &definition, registry, &mut sink)
        .now()
        .unwrap();
    sink.drain();
    let outcomes = session.run_now("INFO FOR REPLICATION east;").unwrap();
    let Some(tessari_session::Outcome::Value(Value::Object(report))) = outcomes.into_iter().next()
    else {
        panic!("INFO FOR REPLICATION did not answer with an object");
    };
    let Some(Value::Object(served)) = report.get("served") else {
        panic!("no served group: {report:?}");
    };
    let Some(Value::Array(confirmed)) = served.get("confirmed") else {
        panic!("no confirmed positions: {served:?}");
    };
    assert!(
        confirmed.iter().any(|row| matches!(row, Value::Object(fields) if fields.contains_key("log") && fields.contains_key("confirmed"))),
        "each row names its log and the position confirmed through: {confirmed:?}"
    );
    // And the registry is the same one the driver advanced: dropping the link
    // empties both views.
    registry.drop_link("east");
    assert!(
        store.links().floors("east").is_empty(),
        "dropping the link forgot the floors"
    );
}

#[test]
fn a_database_received_from_another_cluster_is_read_only_by_declaration() {
    // D7: the named refusal. Not NotWritable — the replica is not "not the
    // leader"; its writer is another cluster, and the refusal says so, because
    // the remedy for the two is different. Refused inside the commit, so a
    // VERIFY rehearses it; refused for the owner as for anybody, because the
    // copy's writer is elsewhere no matter who asks.
    let store = store_with_a_link();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE REPLICATION west FROM 'cluster-a:9080' FOR DATABASE prod.shop; \
             USE NAMESPACE prod DATABASE shop;",
        )
        .unwrap();
    let refused = session
        .run_now("CREATE orders:9 = { item: 'sneaky' };")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("read-only") && refused.contains("west"),
        "the refusal names the class and the link: {refused}"
    );
    // A rehearsal sees the same refusal — the gate runs inside the commit,
    // and `VERIFY` is the rehearsal: every check a COMMIT runs, then nothing
    // is written.
    let rehearsal = session
        .run_now(
            "BEGIN; \
             USE NAMESPACE prod DATABASE shop; \
             CREATE orders:9 = { item: 'sneaky' }; \
             VERIFY;",
        )
        .unwrap_err()
        .to_string();
    assert!(
        rehearsal.contains("read-only"),
        "a VERIFY rehearses the gate: {rehearsal}"
    );
    // And the database beside the replica is writable — the gate is per
    // database, not per store.
    session
        .run_now("USE NAMESPACE prod DATABASE other; CREATE orders:1 = { item: 'pen' };")
        .unwrap();
    // Dropping the link lifts the read-only: the declaration was the gate.
    session.run_now("DROP REPLICATION west;").unwrap();
    session
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:9 = { item: 'book' };")
        .unwrap();
}

#[test]
fn the_direction_is_reported_and_alter_cannot_flip_it() {
    let store = store_with_a_link();
    let mut session = Session::new(&store);
    session
        .run_now("DEFINE REPLICATION west FROM 'cluster-a:9080' FOR DATABASE prod.shop;")
        .unwrap();
    let mut read = |statement: &str| {
        let outcomes = session.run_now(statement).unwrap();
        let Some(tessari_session::Outcome::Value(Value::Object(report))) =
            outcomes.into_iter().next()
        else {
            panic!("{statement} did not answer with an object");
        };
        report
    };
    let west = read("INFO FOR REPLICATION west;");
    let Some(Value::Object(declared)) = west.get("declared") else {
        panic!("no declared group: {west:?}");
    };
    assert_eq!(declared.get("direction"), Some(&Value::from("inbound")));
    // `east` is declared by the fixture, outbound.
    let east = read("INFO FOR REPLICATION east;");
    let Some(Value::Object(declared)) = east.get("declared") else {
        panic!("no declared group: {east:?}");
    };
    assert_eq!(declared.get("direction"), Some(&Value::from("outbound")));
    // Direction is not a clause: flipping it is dropping and re-declaring,
    // because a flipped link is a different link.
    assert!(
        session
            .run_now("ALTER REPLICATION west TO 'cluster-c:9080';")
            .is_err()
    );
}

#[test]
fn a_link_end_to_end_records_land_and_the_copy_is_read_only() {
    // The shape of the whole seam in one process: a source cluster serves its
    // link, a target cluster applies what ships, and the two halves of the
    // contract meet — the DDL crosses with the data, the copy is read-only by
    // declaration on the target, and a write made after the link is up lands
    // on the next pass.
    let source = store_with_a_link();
    let mut writing = Session::new(&source);
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();

    let definition = definition(&source, "east");
    let registry = source.links();
    let mut sink = ChannelSink::new();
    serve_once(&source, &definition, registry, &mut sink)
        .now()
        .unwrap();
    let shipped = sink.drain();
    assert!(!shipped.is_empty(), "the source shipped its database's log");

    // The target starts empty: the schema crossed inside the stream, so the
    // intake declares the namespace, the database, the table and the record
    // in one application — a copy built by the log alone.
    let target = Store::open(backend()).now().unwrap();
    for (_, envelope) in &shipped {
        intake(&target, envelope.clone()).now().unwrap();
    }
    let mut reading = Session::new(&target);
    let outcomes = reading
        .run_now("USE NAMESPACE prod DATABASE shop; SELECT * FROM orders;")
        .unwrap();
    let answer = outcomes.last().expect("the SELECT answers");
    let records = match answer {
        tessari_session::Outcome::Records { records, .. } => records,
        other => panic!("the SELECT did not answer records: {other:?}"),
    };
    assert_eq!(
        records.len(),
        1,
        "the record the source wrote is the record the copy holds"
    );

    // The target declares the inbound half, and the copy becomes read-only
    // by declaration — the write the source took goes nowhere here.
    reading
        .run_now("DEFINE REPLICATION west FROM 'cluster-a:9080' FOR DATABASE prod.shop;")
        .unwrap();
    let refused = reading
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:2 = { item: 'sneaky' };")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("read-only"),
        "the copy refuses its own divergent writes: {refused}"
    );

    // A write made on the source after the link is up ships on the next pass
    // and lands — the stream is alive, and the at-least-once boundary holds:
    // only what the target has not confirmed crosses.
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:3 = { item: 'map' };")
        .unwrap();
    let mut sink = ChannelSink::new();
    serve_once(&source, &definition, registry, &mut sink)
        .now()
        .unwrap();
    let more = sink.drain();
    for (_, envelope) in &more {
        intake(&target, envelope.clone()).now().unwrap();
    }
    assert!(
        !more.is_empty(),
        "the second pass shipped what the first had not seen"
    );
    let outcomes = reading
        .run_now("USE NAMESPACE prod DATABASE shop; SELECT * FROM orders;")
        .unwrap();
    let answer = outcomes.last().expect("the SELECT answers");
    let records = match answer {
        tessari_session::Outcome::Records { records, .. } => records,
        other => panic!("the SELECT did not answer records: {other:?}"),
    };
    assert_eq!(
        records.len(),
        2,
        "the copy holds exactly what the source has written — the refused \
         divergent write is not among them: {records:?}"
    );
}

#[test]
fn a_seeded_declaration_names_its_position_and_reports_seeding() {
    // The seeding lifecycle opens with the declaration: `SEED AT` names the
    // position the receiving copy's snapshot ends at, and the row reports
    // `seeding` until the copy has caught up to that floor. The unseeded
    // link beside it reads as `active` — the lifecycle's absence is today's
    // behavior, not a state of its own.
    let store = store_with_a_link();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE REPLICATION west FROM 'cluster-a:9080' FOR DATABASE prod.shop \
             SEED AT 7;",
        )
        .unwrap();
    let mut read = |statement: &str| {
        let outcomes = session.run_now(statement).unwrap();
        let Some(tessari_session::Outcome::Value(Value::Object(report))) =
            outcomes.into_iter().next()
        else {
            panic!("{statement} did not answer with an object");
        };
        report
    };
    let west = read("INFO FOR REPLICATION west;");
    let Some(Value::Object(progress)) = west.get("progress") else {
        panic!("no progress group: {west:?}");
    };
    assert_eq!(
        progress.get("state"),
        Some(&Value::from("seeding")),
        "a fresh seeded link reports the state it starts in"
    );
    assert_eq!(
        progress.get("seed_at"),
        Some(&Value::from(7)),
        "the declared position reads back"
    );
    // The fixture's `east` was declared without a seed position: it is active
    // by default and carries no seed floor.
    let east = read("INFO FOR REPLICATION east;");
    let Some(Value::Object(progress)) = east.get("progress") else {
        panic!("no progress group: {east:?}");
    };
    assert_eq!(progress.get("state"), Some(&Value::from("active")));
    assert!(
        !progress.contains_key("seed_at"),
        "an unseeded link names no seed position: {progress:?}"
    );
}

#[test]
fn a_seeded_source_serves_the_tail_from_the_seed_position() {
    // The reason the seed position exists on the outbound row: the far side
    // holds everything through the snapshot's end, so the source serves the
    // tail from one past it — the history below the floor does not re-cross.
    let source = store_with_a_link();
    let mut writing = Session::new(&source);
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let held = definition(&source, "east");
    let log = source
        .logs()
        .now()
        .unwrap()
        .into_iter()
        .find(|log| log.home == held.database)
        .expect("the link's own log");
    let floor = source.committed_tail(log).now().unwrap();
    writing
        .run_now(&format!(
            "DROP REPLICATION east; \
             DEFINE REPLICATION east TO 'cluster-b:9080' FOR DATABASE prod.shop \
             SEED AT {floor};"
        ))
        .unwrap();
    // A pass with nothing beyond the floor ships nothing from the link's
    // own log: the confirmed history the snapshot already holds does not
    // re-cross. The parent logs the reach reads re-read from their origins
    // (the copy's already-held positions answer them as no-ops on intake) —
    // the floor is the own log's, the log the seed position counts in.
    let mut quiet = ChannelSink::new();
    serve_once(
        &source,
        &definition(&source, "east"),
        source.links(),
        &mut quiet,
    )
    .now()
    .unwrap();
    assert!(
        quiet
            .drain()
            .iter()
            .all(|(_, envelope)| envelope.log.home != held.database),
        "nothing below the floor crosses the link's own log"
    );
    // A write beyond the floor ships alone, from one past it.
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:2 = { item: 'map' };")
        .unwrap();
    let mut sink = ChannelSink::new();
    serve_once(
        &source,
        &definition(&source, "east"),
        source.links(),
        &mut sink,
    )
    .now()
    .unwrap();
    let shipped = sink.drain();
    let own = shipped
        .iter()
        .filter(|(_, envelope)| envelope.log.home == held.database)
        .collect::<Vec<_>>();
    assert_eq!(own.len(), 1, "one own-log envelope ships: {shipped:?}");
    let (_, envelope) = own[0];
    assert_eq!(
        envelope.from,
        floor
            .get()
            .checked_add(1)
            .map(Sequence::new)
            .expect("the tail fits"),
        "the tail is served from one past the seed position"
    );
    assert_eq!(
        envelope.records.len(),
        1,
        "only the record beyond the floor crosses: {envelope:?}"
    );
}

#[test]
fn a_seeded_copy_promotes_itself_to_standby_when_it_catches_up() {
    // The automatic half of the machine, on the receiving side: the operator
    // restores the snapshot, declares the link with the position the snapshot
    // ended at, and the declaration itself finds the copy already holding that
    // floor — the link reports `standby`, the state that serves reads.
    let source = store_with_a_link();
    let mut writing = Session::new(&source);
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let shipped = definition(&source, "east");
    let mut sink = ChannelSink::new();
    serve_once(&source, &shipped, source.links(), &mut sink)
        .now()
        .unwrap();
    let envelopes = sink.drain();

    let target = Store::open(backend()).now().unwrap();
    for (_, envelope) in &envelopes {
        intake(&target, envelope.clone()).now().unwrap();
    }
    // The restored copy: the schema and the record the source wrote, landed
    // through the intake — the snapshot's end is the tail the copy holds.
    let log = target
        .logs()
        .now()
        .unwrap()
        .into_iter()
        .find(|log| log.home == shipped.database)
        .expect("the copy's own log");
    let floor = target.committed_tail(log).now().unwrap();
    let mut reading = Session::new(&target);
    reading
        .run_now(&format!(
            "DEFINE REPLICATION west FROM 'cluster-a:9080' FOR DATABASE prod.shop \
             SEED AT {floor};"
        ))
        .unwrap();
    let outcomes = reading.run_now("INFO FOR REPLICATION west;").unwrap();
    let Some(tessari_session::Outcome::Value(Value::Object(report))) = outcomes.into_iter().next()
    else {
        panic!("INFO did not answer with an object");
    };
    let Some(Value::Object(progress)) = report.get("progress") else {
        panic!("no progress group: {report:?}");
    };
    assert_eq!(
        progress.get("state"),
        Some(&Value::from("standby")),
        "the declaration found the copy holding the seed floor: {progress:?}"
    );
}

#[test]
fn alter_replication_active_promotes_a_caught_up_standby_link() {
    // The operator's promotion: a standby link whose copy has caught up takes
    // `ALTER REPLICATION … ACTIVE` and reports `active` — the ordinary
    // running state, today's RESUMED behavior named.
    let target = Store::open(backend()).now().unwrap();
    let mut reading = Session::new(&target);
    reading
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
             USE DATABASE shop; DEFINE TABLE orders SCHEMALESS; \
             CREATE orders:1 = { item: 'book' };",
        )
        .unwrap();
    // The copy's tail after its own writes is the floor the snapshot names.
    let tenancy = Reach::Database(NamespaceId::new(1), tessari_types::DatabaseId::new(1));
    let log = target
        .logs()
        .now()
        .unwrap()
        .into_iter()
        .find(|log| log.home == tenancy)
        .expect("the copy's own log");
    let floor = target.committed_tail(log).now().unwrap();
    reading
        .run_now(&format!(
            "DEFINE REPLICATION west FROM 'cluster-a:9080' FOR DATABASE prod.shop \
             SEED AT {floor};"
        ))
        .unwrap();
    reading.run_now("ALTER REPLICATION west ACTIVE;").unwrap();
    let outcomes = reading.run_now("INFO FOR REPLICATION west;").unwrap();
    let Some(tessari_session::Outcome::Value(Value::Object(report))) = outcomes.into_iter().next()
    else {
        panic!("INFO did not answer with an object");
    };
    let Some(Value::Object(progress)) = report.get("progress") else {
        panic!("no progress group: {report:?}");
    };
    assert_eq!(
        progress.get("state"),
        Some(&Value::from("active")),
        "the early-promote spelling moves a caught-up link to active"
    );
}

#[test]
fn alter_replication_active_refuses_while_the_copy_is_still_seeding_and_names_the_floor() {
    // The gate's first refusal arm: a copy that has not reached the seed
    // position cannot be promoted, and the refusal names the floor it stands
    // at — the operator's next act is written in the error.
    let target = Store::open(backend()).now().unwrap();
    let mut reading = Session::new(&target);
    reading
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
             USE DATABASE shop; DEFINE TABLE orders SCHEMALESS; \
             CREATE orders:1 = { item: 'book' };",
        )
        .unwrap();
    let tenancy = Reach::Database(NamespaceId::new(1), tessari_types::DatabaseId::new(1));
    let log = target
        .logs()
        .now()
        .unwrap()
        .into_iter()
        .find(|log| log.home == tenancy)
        .expect("the copy's own log");
    let tail = target.committed_tail(log).now().unwrap();
    // A seed position five past what the copy holds: still seeding.
    let floor = tail
        .get()
        .checked_add(5)
        .map(Sequence::new)
        .expect("the floor fits");
    reading
        .run_now(&format!(
            "DEFINE REPLICATION west FROM 'cluster-a:9080' FOR DATABASE prod.shop \
             SEED AT {floor};"
        ))
        .unwrap();
    let refused = reading
        .run_now("ALTER REPLICATION west ACTIVE;")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("seeding") && refused.contains(&floor.get().to_string()),
        "the refusal names the state and the floor: {refused}"
    );
}

#[test]
fn alter_replication_active_on_the_source_refuses_while_the_tail_runs_ahead() {
    // The gate's second refusal arm, on the outbound row: the source knows
    // both numbers — what the far side has confirmed and how far its own tail
    // has run — and the early-promote refuses while the tail runs ahead,
    // naming the lag. One more serve closes it, and the spelling then takes.
    let source = store_with_a_link();
    let mut writing = Session::new(&source);
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let held = definition(&source, "east");
    let log = source
        .logs()
        .now()
        .unwrap()
        .into_iter()
        .find(|log| log.home == held.database)
        .expect("the link's own log");
    let floor = source.committed_tail(log).now().unwrap();
    writing
        .run_now(&format!(
            "DROP REPLICATION east; \
             DEFINE REPLICATION east TO 'cluster-b:9080' FOR DATABASE prod.shop \
             SEED AT {floor};"
        ))
        .unwrap();
    let mut sink = ChannelSink::new();
    serve_once(
        &source,
        &definition(&source, "east"),
        source.links(),
        &mut sink,
    )
    .now()
    .unwrap();
    sink.drain();
    // The first serve confirmed through the seed position: the source-side
    // transition fires, and the row reports standby before the operator
    // asks anything.
    let outcomes = writing.run_now("INFO FOR REPLICATION east;").unwrap();
    let Some(tessari_session::Outcome::Value(Value::Object(report))) = outcomes.into_iter().next()
    else {
        panic!("INFO did not answer with an object");
    };
    let Some(Value::Object(progress)) = report.get("progress") else {
        panic!("no progress group: {report:?}");
    };
    assert_eq!(
        progress.get("state"),
        Some(&Value::from("standby")),
        "the serve that confirmed the seed floor promoted the link: {progress:?}"
    );
    // The source writes once more: the tail runs one ahead of the confirmed
    // floor until the next pass ships it.
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:2 = { item: 'map' };")
        .unwrap();
    let refused = writing
        .run_now("ALTER REPLICATION east ACTIVE;")
        .unwrap_err()
        .to_string();
    let lag = 1;
    assert!(
        refused.contains(&lag.to_string()),
        "the refusal names the lag: {refused}"
    );
    let mut sink = ChannelSink::new();
    serve_once(
        &source,
        &definition(&source, "east"),
        source.links(),
        &mut sink,
    )
    .now()
    .unwrap();
    sink.drain();
    writing.run_now("ALTER REPLICATION east ACTIVE;").unwrap();
    let outcomes = writing.run_now("INFO FOR REPLICATION east;").unwrap();
    let Some(tessari_session::Outcome::Value(Value::Object(report))) = outcomes.into_iter().next()
    else {
        panic!("INFO did not answer with an object");
    };
    let Some(Value::Object(progress)) = report.get("progress") else {
        panic!("no progress group: {report:?}");
    };
    assert_eq!(
        progress.get("state"),
        Some(&Value::from("active")),
        "at zero lag the spelling promotes the link: {progress:?}"
    );
}

#[test]
fn an_unseeded_link_reads_active_and_the_promote_spelling_is_a_no_op() {
    // The default the lifecycle preserves: a link declared without a seed
    // position is active from birth, and promoting what is already active
    // answers ok and changes nothing.
    let store = store_with_a_link();
    let mut session = Session::new(&store);
    session.run_now("ALTER REPLICATION east ACTIVE;").unwrap();
    let outcomes = session.run_now("INFO FOR REPLICATION east;").unwrap();
    let Some(tessari_session::Outcome::Value(Value::Object(report))) = outcomes.into_iter().next()
    else {
        panic!("INFO did not answer with an object");
    };
    let Some(Value::Object(progress)) = report.get("progress") else {
        panic!("no progress group: {report:?}");
    };
    assert_eq!(progress.get("state"), Some(&Value::from("active")));
}

#[test]
fn an_own_log_shipment_from_below_the_seed_position_is_refused_and_names_the_floor() {
    // The seed's tail-half honesty, from §C's CutShort pattern: the copy
    // declares it holds through the seed position, so a shipment whose own
    // log starts at or below that floor claims the snapshot the copy never
    // took — the seed and the source disagree, and the intake refuses
    // naming both numbers instead of silently converging over a lying base.
    let source = store_with_a_link();
    let mut writing = Session::new(&source);
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let shipped = definition(&source, "east");
    let mut sink = ChannelSink::new();
    serve_once(&source, &shipped, source.links(), &mut sink)
        .now()
        .unwrap();
    let envelopes = sink.drain();

    let target = Store::open(backend()).now().unwrap();
    for (_, envelope) in &envelopes {
        intake(&target, envelope.clone()).now().unwrap();
    }
    let log = target
        .logs()
        .now()
        .unwrap()
        .into_iter()
        .find(|log| log.home == shipped.database)
        .expect("the copy's own log");
    let held = target.committed_tail(log).now().unwrap();
    // The receiver overclaims: a seed position two past what its copy holds.
    let floor = held
        .get()
        .checked_add(2)
        .map(Sequence::new)
        .expect("the floor fits");
    let mut reading = Session::new(&target);
    reading
        .run_now(&format!(
            "DEFINE REPLICATION west FROM 'cluster-a:9080' FOR DATABASE prod.shop \
             SEED AT {floor};"
        ))
        .unwrap();
    // The source serves its next record from one past what the copy holds —
    // below the floor the receiver declared.
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:2 = { item: 'map' };")
        .unwrap();
    let mut sink = ChannelSink::new();
    serve_once(
        &source,
        &definition(&source, "east"),
        source.links(),
        &mut sink,
    )
    .now()
    .unwrap();
    let envelopes = sink.drain();
    let (_, overlap) = envelopes
        .iter()
        .find(|(_, envelope)| envelope.log.home == shipped.database)
        .expect("the source shipped its own log");
    let refused = intake(&target, overlap.clone()).now().unwrap_err();
    let text = refused.to_string();
    let from = overlap.from.get().to_string();
    let floor = floor.get().to_string();
    assert!(
        text.contains(&from) && text.contains(&floor),
        "the refusal names the shipment's start and the seed floor: {text}"
    );
}

#[test]
fn a_seed_floor_pruned_below_refuses_the_serve_naming_the_log_start() {
    // The seed's source-half honesty: a seeded link serves the tail from
    // one past the seed position — and when the source has pruned its log
    // below that floor, the read refuses BelowLogStart, naming the asked
    // position and the log's surviving start. The far side's remedy is
    // written in the refusal: a fresh copy of the state. (The pruner keeps
    // a live link's confirmed floors by itself, so the disaster shape here
    // is the honest one: the link was dropped, the log pruned past the old
    // seed, and the link re-declared at a position the log no longer
    // holds.)
    let source = store_with_a_link();
    let mut writing = Session::new(&source);
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let held = definition(&source, "east");
    let log = source
        .logs()
        .now()
        .unwrap()
        .into_iter()
        .find(|log| log.home == held.database)
        .expect("the link's own log");
    let floor = source.committed_tail(log).now().unwrap();
    writing
        .run_now(&format!(
            "DROP REPLICATION east; \
             DEFINE REPLICATION east TO 'cluster-b:9080' FOR DATABASE prod.shop \
             SEED AT {floor};"
        ))
        .unwrap();
    let mut sink = ChannelSink::new();
    serve_once(
        &source,
        &definition(&source, "east"),
        source.links(),
        &mut sink,
    )
    .now()
    .unwrap();
    sink.drain();
    // The tail moves twice; then the link is dropped — its confirmed floors
    // die with it — and the source prunes the two records the far side
    // never confirmed (the pruner keeps the tail record by itself, so the
    // prune stops one below it). Re-declaring at the old seed names a floor
    // the log no longer holds.
    writing
        .run_now(
            "USE NAMESPACE prod DATABASE shop; CREATE orders:2 = { item: 'map' }; \
             CREATE orders:3 = { item: 'atlas' };",
        )
        .unwrap();
    let tail = source.committed_tail(log).now().unwrap();
    let upto = Sequence::new(tail.get().saturating_sub(1));
    source.links().drop_link("east");
    let pruned = source.prune_log(log, upto).now().expect("the prune lands");
    writing.run_now("DROP REPLICATION east;").unwrap();
    writing
        .run_now(&format!(
            "DEFINE REPLICATION east TO 'cluster-b:9080' FOR DATABASE prod.shop \
             SEED AT {floor};"
        ))
        .unwrap();
    let refused = serve_once(
        &source,
        &definition(&source, "east"),
        source.links(),
        &mut ChannelSink::new(),
    )
    .now()
    .unwrap_err()
    .to_string();
    let asked = floor
        .get()
        .checked_add(1)
        .map(|held| held.to_string())
        .expect("the asked position fits");
    let start = pruned.start.get().to_string();
    assert!(
        refused.contains(&asked) && refused.contains(&start),
        "the refusal names the asked position and the log's surviving start: {refused}"
    );
}

#[test]
fn the_loop_honors_pause_between_passes_and_ships_again_on_resume() {
    // The operability contract, live: the catalog is re-read every pass, so
    // PAUSED holds the stream at the next tick rather than at the next
    // restart — a stopped stream is a statement, not a process to hunt down.
    let source = store_with_a_link();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    // One queue, two handles: `seen` drains what the loop's handle ships.
    let seen = ChannelSink::new();
    // The spawn needs a runtime context; the sessions must NOT run inside one
    // (`run_now` drives its own future, and a future driven inside a runtime
    // is the panic `now` exists to catch). So: spawn in one small drive, do
    // the synchronous work while the shared test runtime runs the loop on its
    // workers, and stop in a last drive. The runtime outlives each drive —
    // that is what the shared runtime is for.
    // `poll_fn`, not an `async` block: the block's output would be the
    // JoinHandle itself, and an async construct yielding an awaitable is the
    // lint this sidesteps. The closure is polled once and answers at once.
    let task = std::future::poll_fn({
        let source = source.clone();
        let seen = seen.clone();
        move |_| {
            // The clones move into this one poll's spawn; `poll_fn` is polled
            // once and the queue is shared behind the handles, so nothing is
            // lost if it were polled again.
            let mut serving = seen.clone();
            let stop_rx = stop_rx.clone();
            let source = source.clone();
            std::task::Poll::Ready(tokio::spawn(async move {
                serve_links(&source, &mut serving, stop_rx).await
            }))
        }
    })
    .now();
    let mut session = Session::new(&source);
    session
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    // The loop ships on its tick; poll with a deadline rather than sleeping a
    // fixed hope.
    let landed = (0..250).any(|_| {
        if !seen.drain().is_empty() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        false
    });
    assert!(landed, "the loop shipped what was written");

    // Pause, write, and wait well past several ticks: nothing ships.
    session.run_now("ALTER REPLICATION east PAUSED;").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(60));
    seen.drain();
    session
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:2 = { item: 'map' };")
        .unwrap();
    let mut idle = true;
    for _ in 0..15 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        if !seen.drain().is_empty() {
            idle = false;
            break;
        }
    }
    assert!(idle, "a paused link holds the stream at the next pass");

    // Resume, and the same pass picks the stream up where the registry left
    // it — the confirmation boundary, not a re-ship of what crossed already.
    session.run_now("ALTER REPLICATION east RESUMED;").unwrap();
    let resumed = (0..250).any(|_| {
        if !seen.drain().is_empty() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        false
    });
    assert!(resumed, "a resumed link ships again");
    (async {
        stop_tx.send(true).ok();
        task.await
    })
    .now()
    .unwrap()
    .unwrap();
}

#[test]
fn the_loop_persists_the_confirmed_position_and_a_fresh_loop_resumes_from_it() {
    // The cursor round-trip: a confirming run leaves the floor in the row, and
    // the next run seeds the registry from the row — a restart resumes near
    // where the relationship stood instead of re-shipping from the origin.
    let source = store_with_a_link();
    let mut session = Session::new(&source);
    session
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let definition = definition(&source, "east");
    let mut sink = ChannelSink::new();
    serve_once(&source, &definition, source.links(), &mut sink)
        .now()
        .unwrap();
    let floor = source.links().floors("east");
    assert!(!floor.is_empty(), "the pass confirmed a floor");
    // The cursor that matters is the own log's — the log whose home is the
    // link's database reach — beside the parent (store) log's floor the same
    // pass also holds.
    let (own_log, own_floor) = floor
        .iter()
        .copied()
        .find(|(log, _)| log.home == definition.database)
        .expect("the own log's floor");

    // Stop the loop before it has served a tick: the stop itself lands the cursor.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let seen = ChannelSink::new();
    let task = std::future::poll_fn({
        let source = source.clone();
        move |_| {
            let mut serving = seen.clone();
            let stop_rx = stop_rx.clone();
            let source = source.clone();
            std::task::Poll::Ready(tokio::spawn(async move {
                serve_links(&source, &mut serving, stop_rx).await
            }))
        }
    })
    .now();
    (async {
        stop_tx.send(true).ok();
        task.await
    })
    .now()
    .unwrap()
    .unwrap();

    let mut reading = source.begin().now().unwrap();
    let row = Catalog::new(&mut reading)
        .replications()
        .now()
        .unwrap()
        .into_iter()
        .find(|held| held.name == "east")
        .expect("the row");
    assert_eq!(
        row.position,
        Some(own_floor),
        "the stop wrote the confirmed floor to the row"
    );
    drop(reading);

    // A fresh loop seeds from the row: a write after the seed ships alone —
    // the pass resumes one past the cursor instead of re-reading the confirmed
    // history.
    source.links().drop_link("east");
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let seen = ChannelSink::new();
    let task = std::future::poll_fn({
        let source = source.clone();
        let seen = seen.clone();
        move |_| {
            let mut serving = seen.clone();
            let stop_rx = stop_rx.clone();
            let source = source.clone();
            std::task::Poll::Ready(tokio::spawn(async move {
                serve_links(&source, &mut serving, stop_rx).await
            }))
        }
    })
    .now();
    session
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:2 = { item: 'map' };")
        .unwrap();
    let shipped = (0..250).find_map(|_| {
        let found = seen
            .drain()
            .into_iter()
            .find(|(_, envelope)| envelope.log == own_log)
            .map(|(_, envelope)| envelope);
        if found.is_some() {
            return found;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        None
    });
    let shipped = shipped.expect("the seeded loop served the link's own log");
    assert_eq!(
        shipped.from,
        Sequence::new(own_floor.get().saturating_add(1)),
        "the pass resumed one past the persisted cursor, not at the origin"
    );
    assert_eq!(
        shipped.records.len(),
        1,
        "the seeded pass shipped only the record the cursor had not seen"
    );
    (async {
        stop_tx.send(true).ok();
        task.await
    })
    .now()
    .unwrap()
    .unwrap();
}

#[test]
fn a_batch_applied_twice_leaves_the_copy_unchanged() {
    // C2's injection proof: at-least-once means a batch CAN arrive twice, and
    // idempotent application is what makes that a bookkeeping fact instead of
    // a corruption. Applying a shipped envelope a second time is a no-op — the
    // apply path answers an already-held position by its divergence check,
    // and an identical record is not a divergence.
    let source = store_with_a_link();
    let mut writing = Session::new(&source);
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let definition = definition(&source, "east");
    let mut sink = ChannelSink::new();
    serve_once(&source, &definition, source.links(), &mut sink)
        .now()
        .unwrap();
    let shipped = sink.drain();
    let target = Store::open(backend()).now().unwrap();
    for (_, envelope) in &shipped {
        intake(&target, envelope.clone()).now().unwrap();
        // The re-delivery: the same bytes, a second time.
        intake(&target, envelope.clone()).now().unwrap();
    }
    let mut reading = Session::new(&target);
    let outcomes = reading
        .run_now("USE NAMESPACE prod DATABASE shop; SELECT * FROM orders;")
        .unwrap();
    let Some(tessari_session::Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("the SELECT did not answer records");
    };
    assert_eq!(
        records.len(),
        1,
        "a batch applied twice landed once: {records:?}"
    );
}

#[test]
fn a_shipped_empty_record_at_an_applied_position_is_a_noop_not_a_divergence() {
    // The clustered-receiver rule in miniature: a receiving side holds its OWN record
    // at position 1 of the line log — every cluster writes its membership and
    // its leadership renewals there — while the shipment offers the source's
    // record at the same position filtered to nothing, because the record is
    // outside the link's reach. Both records say the reach has no content at
    // this position; the copy's own record stands and the confirmation moves.
    let target = Store::open(backend()).now().unwrap();
    let mut session = Session::new(&target);
    session
        .run_now("DEFINE NAMESPACE own; DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; DEFINE TABLE orders SCHEMALESS;")
        .unwrap();
    let line = target
        .logs()
        .now()
        .unwrap()
        .into_iter()
        .find(|log| log.home == Reach::Store)
        .expect("the store's own store log");
    let envelope = Envelope {
        origin: [7_u8; 16],
        log: line,
        from: Sequence::new(1),
        records: vec![(
            Sequence::new(1),
            tessari_encoding::LogRecord::new(Vec::new()),
        )],
    };
    let applied = intake(&target, envelope).now().unwrap();
    assert_eq!(
        applied,
        Sequence::new(1),
        "the confirmation answers the position"
    );
    let mut reading = Session::new(&target);
    let outcomes = reading
        .run_now("USE NAMESPACE own; INFO FOR NAMESPACE;")
        .unwrap();
    assert!(
        matches!(
            outcomes.last(),
            Some(tessari_session::Outcome::Value { .. })
        ),
        "the copy's own record at that position stands — the namespace resolves: {outcomes:?}"
    );
}

#[test]
fn a_shipped_empty_record_at_a_fresh_position_keeps_the_arithmetic() {
    // The other half of that rule: skipping is only for already-applied
    // positions. An empty marker at a fresh position still extends the log —
    // positions are arithmetic, and the record after it must not read as a gap.
    let target = Store::open(backend()).now().unwrap();
    let mut session = Session::new(&target);
    session
        .run_now("DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; DEFINE TABLE orders SCHEMALESS; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let line = target
        .logs()
        .now()
        .unwrap()
        .into_iter()
        .find(|log| {
            log.home == Reach::Database(NamespaceId::new(1), tessari_types::DatabaseId::new(1))
        })
        .expect("the store's own database log");
    let write = |at: u64, text: &str| {
        tessari_encoding::LogRecord::new(vec![tessari_encoding::Mutation {
            namespace: NamespaceId::new(1),
            database: tessari_types::DatabaseId::new(1),
            table: tessari_types::TableId::new(1),
            id: tessari_types::RecordId::from(format!("orders:{at}")),
            shard: None,
            value: tessari_encoding::StampedValue::new(tessari_encoding::RecordValue::Present(
                tessari_encoding::encode_payload(&tessari_types::Value::from(text)).into_bytes(),
            )),
        }])
    };
    let envelope = Envelope {
        origin: [7_u8; 16],
        log: line,
        from: Sequence::new(2),
        records: vec![
            (
                Sequence::new(2),
                tessari_encoding::LogRecord::new(Vec::new()),
            ),
            (Sequence::new(3), write(2, "shipped")),
        ],
    };
    let applied = intake(&target, envelope).now().unwrap();
    assert_eq!(
        applied,
        Sequence::new(3),
        "the empty marker extended the log, no gap"
    );
    let mut reading = Session::new(&target);
    let outcomes = reading
        .run_now("USE NAMESPACE prod DATABASE shop; SELECT * FROM orders;")
        .unwrap();
    let Some(tessari_session::Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("the SELECT did not answer records");
    };
    assert_eq!(records.len(), 2, "the record after the marker landed");
}

#[test]
fn dropping_the_link_stops_the_stream_between_passes() {
    let source = store_with_a_link();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let seen = ChannelSink::new();
    let task = std::future::poll_fn({
        let source = source.clone();
        let seen = seen.clone();
        move |_| {
            let mut serving = seen.clone();
            let stop_rx = stop_rx.clone();
            let source = source.clone();
            std::task::Poll::Ready(tokio::spawn(async move {
                serve_links(&source, &mut serving, stop_rx).await
            }))
        }
    })
    .now();
    let mut session = Session::new(&source);
    session
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let shipped = (0..250).any(|_| {
        if !seen.drain().is_empty() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        false
    });
    assert!(shipped, "the loop shipped before the drop");

    session.run_now("DROP REPLICATION east;").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(60));
    seen.drain();
    session
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:2 = { item: 'map' };")
        .unwrap();
    let mut quiet = true;
    for _ in 0..15 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        if !seen.drain().is_empty() {
            quiet = false;
            break;
        }
    }
    assert!(quiet, "a dropped link stops shipping at the next pass");
    (async {
        stop_tx.send(true).ok();
        task.await
    })
    .now()
    .unwrap()
    .unwrap();
}

#[test]
fn the_plural_lists_every_link_for_the_manager_that_has_no_name_yet() {
    // The console asks before it has a name to look up. Name order, so two
    // nodes comparing what they think the cluster says can compare in it.
    let store = store_with_a_link();
    let mut session = Session::new(&store);
    session
        .run_now("DEFINE REPLICATION west FROM 'cluster-a:9080' FOR DATABASE prod.shop;")
        .unwrap();
    let outcomes = session.run_now("INFO FOR REPLICATIONS;").unwrap();
    let Some(tessari_session::Outcome::Value(Value::Object(report))) = outcomes.into_iter().next()
    else {
        panic!("INFO FOR REPLICATIONS did not answer with an object");
    };
    let Some(Value::Array(rows)) = report.get("replications") else {
        panic!("no replications list: {report:?}");
    };
    let names: Vec<String> = rows
        .iter()
        .filter_map(|row| match row {
            Value::Object(fields) => match fields.get("name") {
                Some(Value::String(name)) => Some(name.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["east".to_owned(), "west".to_owned()]);
    let directions: Vec<&str> = rows
        .iter()
        .filter_map(|row| match row {
            Value::Object(fields) => match fields.get("direction") {
                Some(Value::String(held)) => Some(held.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(directions, vec!["outbound", "inbound"]);
}

#[test]
fn a_seeded_link_serves_only_the_log_its_seed_names() {
    // The survivor shape: this cluster holds two logs of one home — the one it
    // received (the dead primary's) and its own, the history it now originates.
    // A seeded link names one position, and positions count in one log, so the
    // serve reads the log the seed names — the same log the position surface
    // names — and never the received one: the far side's snapshot already
    // holds every effect the received log carries, and shipping it re-sends
    // state the far side cannot apply below its snapshot-restored start.
    let source = store_with_a_link();
    let mut writing = Session::new(&source);
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:1 = { item: 'book' };")
        .unwrap();
    let shipped = definition(&source, "east");
    let mut sink = ChannelSink::new();
    serve_once(&source, &shipped, source.links(), &mut sink)
        .now()
        .unwrap();
    let envelopes = sink.drain();

    // The survivor: it received the primary's log, then the primary died.
    let survivor = Store::open(backend()).now().unwrap();
    for (_, envelope) in &envelopes {
        intake(&survivor, envelope.clone()).now().unwrap();
    }
    let received = survivor
        .logs()
        .now()
        .unwrap()
        .into_iter()
        .find(|log| log.home == shipped.database)
        .expect("the received log");
    // One more record the survivor received: the received log's tail now runs
    // past the seed floor the survivor's own log will name — the shape that
    // distinguishes *serving the received log* from *serving nothing*.
    writing
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:2 = { item: 'map' };")
        .unwrap();
    let mut sink = ChannelSink::new();
    serve_once(&source, &shipped, source.links(), &mut sink)
        .now()
        .unwrap();
    for (_, envelope) in &sink.drain() {
        intake(&survivor, envelope.clone()).now().unwrap();
    }
    // Before any inbound declaration the copy is writable: its own history
    // begins — one record in its own log.
    let mut local = Session::new(&survivor);
    local
        .run_now("USE NAMESPACE prod DATABASE shop; CREATE orders:9 = { item: 'atlas' };")
        .unwrap();
    let own = survivor.own_log(shipped.database).now().unwrap();
    assert_ne!(
        received, own,
        "the survivor holds two logs of the home, the received one and its own"
    );
    // The dead link is gone; the survivor turns source, seeded from its own
    // log's tail — the position its position report names.
    let floor = survivor.committed_tail(own).now().unwrap();
    local
        .run_now(&format!(
            "DEFINE REPLICATION south TO 'cluster-c:9080' FOR DATABASE prod.shop \
             SEED AT {floor};"
        ))
        .unwrap();
    let mut sink = ChannelSink::new();
    serve_once(
        &survivor,
        &definition(&survivor, "south"),
        survivor.links(),
        &mut sink,
    )
    .now()
    .unwrap();
    let served = sink.drain();
    assert!(
        served.iter().all(|(_, envelope)| envelope.log != received),
        "the received log does not cross: {served:?}"
    );
    assert!(
        served
            .iter()
            .filter(|(_, envelope)| envelope.log.home == shipped.database)
            .all(|(_, envelope)| envelope.log == own),
        "the home's served envelopes name the seed's log alone: {served:?}"
    );
}

#[test]
fn an_inbound_link_survives_a_store_reopen() {
    // The receiver's registry is derived from the log records the link's
    // declarations wrote, and a fresh open used to start it empty: nothing
    // re-derived it, so a restarted receiver refused its own link's
    // shipments ('prod.shop is not received from another cluster') and the
    // write gate over a received database went blind — divergence's quietest
    // shape, found by the live seeding exercise. The registry is rebuilt
    // from the catalog at open.
    let backend = backend();
    let store = Store::open(Arc::clone(&backend)).now().unwrap();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
             USE DATABASE shop; DEFINE TABLE orders SCHEMALESS; \
             USE NAMESPACE prod DATABASE shop; \
             DEFINE REPLICATION west FROM 'cluster-a:9080' FOR DATABASE prod.shop;",
        )
        .unwrap();
    let declared = definition(&store, "west");
    let Reach::Database(namespace, database) = declared.database else {
        panic!("the link names a database");
    };
    drop(store);
    let reopened = Store::open(Arc::clone(&backend)).now().unwrap();
    assert_eq!(
        reopened
            .replicated()
            .link_over((namespace, database))
            .as_deref(),
        Some("west"),
        "a reopened store knows what it receives"
    );
}
