//! `DEFINE INDEX … SPATIAL`, end to end.
//!
//! The wave that built the spatial entry built its language surface in the same
//! wave, deliberately: a key grammar with no statement that reaches it is an
//! island, and an island is where a layer gets built against nobody's
//! requirements and is wrong in a way nothing notices.
//!
//! What is asserted here is the **surface**: that the word parses, that it is
//! contextual, that it cannot be combined with another index kind, and that a
//! definition survives the round trip through the catalog. Whether the entries
//! it writes are the right ones is asserted where it can actually be seen — the
//! bidirectional sweep in `tessari-storage`, because a read confirms every
//! candidate against its record and therefore cannot see a broken index.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_encoding::{
    IndexAddress, KeyKind, SpatialExtent, SpatialIndexKey, StoreKey, StoreValue,
};
use tessari_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest, WriteBatch};
use tessari_session::Session;
use tessari_storage::{Catalog, Store};

fn store() -> Store {
    Store::open(backend()).unwrap()
}

fn backend() -> Arc<dyn KvBackend> {
    Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE atlas; USE NAMESPACE atlas;\n\
             DEFINE DATABASE world; USE DATABASE world;\n\
             DEFINE COLLECTION places;",
        )
        .unwrap();
    session
}

#[test]
fn a_spatial_index_is_defined_and_reads_back_as_one() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_where ON places FIELDS location SPATIAL;")
        .unwrap();

    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let namespace = catalog.namespace_id("atlas").unwrap().unwrap();
    let database = catalog.database_id(namespace, "world").unwrap().unwrap();
    let table = catalog
        .table_id(namespace, database, "places")
        .unwrap()
        .unwrap();
    let defined = catalog.indexes_on(table).unwrap();
    let held = defined
        .iter()
        .find(|index| index.name == "by_where")
        .expect("the index reaches the catalog");
    assert!(held.spatial, "it should read back as a spatial index");
    assert!(!held.search && !held.unique && held.vector.is_none());
}

#[test]
fn the_index_is_built_over_rows_that_are_already_there() {
    // The same rule every other kind follows: a definition over a populated
    // table indexes what is there, in the commit that defines it. Without it an
    // index would be visible and empty, and a reader served by it would answer
    // with fewer rows and raise nothing — so this counts the entries rather than
    // checking that the statement returned without an error, which a definition
    // that built nothing at all would also do.
    let held = backend();
    let store = Store::open(Arc::clone(&held)).unwrap();
    let mut session = ready(&store);
    session
        .run(
            "CREATE places:1 = { location: geometry { type: 'Point', coordinates: [2.35, 48.85] } };\n\
             CREATE places:2 = { location: geometry { type: 'LineString', coordinates: [[0, 0], [1, 1]] } };\n\
             CREATE places:3 = { note: 'no geometry here' };",
        )
        .unwrap();
    session
        .run("DEFINE INDEX by_where ON places FIELDS location SPATIAL;")
        .unwrap();

    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let namespace = catalog.namespace_id("atlas").unwrap().unwrap();
    let database = catalog.database_id(namespace, "world").unwrap().unwrap();
    let table = catalog
        .table_id(namespace, database, "places")
        .unwrap()
        .unwrap();
    let index = catalog
        .indexes_on(table)
        .unwrap()
        .into_iter()
        .find(|index| index.name == "by_where")
        .unwrap();
    drop(transaction);

    let address = IndexAddress::new(index.namespace, index.database, index.table, index.id);
    let prefix = address.prefix(KeyKind::SpatialIndex);
    let found = held
        .scan(&ScanRequest {
            keyspace: KeyKind::SpatialIndex.keyspace(),
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Forward,
            limit: None,
        })
        .unwrap();
    assert!(
        found.len() >= 2,
        "two of the three rows carry a geometry, so the build should have written \
         at least one cell for each; found {}",
        found.len()
    );
}

#[test]
fn spatial_is_contextual_and_is_still_a_name() {
    // The same choice `vector` made: a database of geometry is full of fields
    // called `spatial`, and taking the word away would break scripts that never
    // asked for an index.
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE places:1 = { spatial: 'a word, not an index kind' };")
        .unwrap();
    session.run("DEFINE COLLECTION spatial;").unwrap();
}

#[test]
fn an_index_is_one_kind_and_not_two() {
    let store = store();
    let mut session = ready(&store);
    for statement in [
        "DEFINE INDEX bad ON places FIELDS location SPATIAL UNIQUE;",
        "DEFINE INDEX bad ON places FIELDS location SEARCH SPATIAL;",
        "DEFINE INDEX bad ON places FIELDS location SPATIAL VECTOR cosine;",
    ] {
        assert!(
            session.run(statement).is_err(),
            "two kinds on one index should be refused: {statement}"
        );
    }
}

#[test]
fn a_multi_valued_route_is_refused() {
    // A covering is computed from one geometry. A route reaching several would
    // have to mean either a box around all of them or one entry set per
    // element, and the two answer different queries.
    let store = store();
    let mut session = ready(&store);
    assert!(
        session
            .run("DEFINE INDEX bad ON places FIELDS spots[*] SPATIAL;")
            .is_err()
    );
}

#[test]
fn a_rebuild_removes_an_entry_the_rows_do_not_imply() {
    // What a rebuild is for, and the one path the per-mutation writes cannot
    // reach: an entry that is there for no reason any row gives. The write path
    // removes a record's old cells when its geometry changes, so an orphan
    // cannot be produced through the language — it is planted here, because a
    // clear that skipped this key kind would otherwise leave every such entry in
    // place and nothing would ever say so.
    let held = backend();
    let store = Store::open(Arc::clone(&held)).unwrap();
    let mut session = ready(&store);
    session
        .run(
            "CREATE places:1 = { location: geometry { type: 'Point', coordinates: [2.35, 48.85] } };\n\
             DEFINE INDEX by_where ON places FIELDS location SPATIAL;",
        )
        .unwrap();

    let address = index_address(&store);
    let orphan = SpatialIndexKey::new(
        address,
        tessari_geo::Cell::root(),
        tessari_types::RecordId::from("nowhere"),
    );
    held.apply(
        WriteBatch::new().put(
            SpatialIndexKey::keyspace(),
            orphan.encode(),
            SpatialExtent::new(tessari_geo::Bounds::of_position(
                tessari_geo::Snapped::from_units(0, 0).unwrap(),
            ))
            .encode(),
        ),
    )
    .unwrap();
    assert!(entries(&held, address).contains(&orphan.encode().as_slice().to_vec()));

    session.run("REBUILD INDEX by_where ON places;").unwrap();
    assert!(
        !entries(&held, address).contains(&orphan.encode().as_slice().to_vec()),
        "a rebuild should leave only the entries the rows imply"
    );
    assert!(
        !entries(&held, address).is_empty(),
        "and it should leave those — a rebuild that cleared everything is not one"
    );
}

/// The address of the `by_where` index, once it exists.
fn index_address(store: &Store) -> IndexAddress {
    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let namespace = catalog.namespace_id("atlas").unwrap().unwrap();
    let database = catalog.database_id(namespace, "world").unwrap().unwrap();
    let table = catalog
        .table_id(namespace, database, "places")
        .unwrap()
        .unwrap();
    let index = catalog
        .indexes_on(table)
        .unwrap()
        .into_iter()
        .find(|index| index.name == "by_where")
        .unwrap();
    IndexAddress::new(index.namespace, index.database, index.table, index.id)
}

/// Every spatial entry one index holds.
fn entries(held: &Arc<dyn KvBackend>, address: IndexAddress) -> Vec<Vec<u8>> {
    let prefix = address.prefix(KeyKind::SpatialIndex);
    held.scan(&ScanRequest {
        keyspace: KeyKind::SpatialIndex.keyspace(),
        range: KeyRange::prefix(&prefix),
        direction: ScanDirection::Forward,
        limit: None,
    })
    .unwrap()
    .into_iter()
    .map(|(key, _)| key.as_slice().to_vec())
    .collect()
}
