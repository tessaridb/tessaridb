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
    Store, serve_once,
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
