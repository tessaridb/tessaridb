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
