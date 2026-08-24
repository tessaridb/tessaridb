//! Shapes crossing into the store, through the door every record write uses.
//!
//! The geometry crate's own tests establish that the boundary decides correctly.
//! These establish that a record write actually goes through it — which is a
//! different claim, and the one that was false before this wave: the validity
//! check existed, was correct, and was called from nowhere.
//!
//! Two properties matter most here and neither is visible from the geometry
//! crate:
//!
//! - the gate fires on a **schemaless** table, where nothing declares `TYPE
//!   geometry`. That is the store's default table and the shape most geometry
//!   will arrive in, so a check that only ran behind a declaration would be a
//!   check that mostly did not run;
//! - the stored value is the **snapped** one, so a read gives back what a second
//!   write would produce. Without that, a read-modify-write cycle moves vertices
//!   a little every time — invisibly per cycle, and fatally in aggregate.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Parameters, Session};
use tessari_storage::Store;
use tessari_types::{Geometry, Polygon, Position, Ring, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A namespace, a database, and a table nobody declared a single field on.
///
/// `DEFINE TABLE` with no `SCHEMAFULL` is the store's default: it constrains
/// nothing, and the storage layer's schema check returns early for it. If the
/// geometry gate lived there, every test in this file would pass vacuously.
fn schemaless(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE maps; USE NAMESPACE maps;\n\
             DEFINE DATABASE world; USE DATABASE world;\n\
             DEFINE TABLE places;",
        )
        .unwrap();
    session
}

fn bound(name: &str, value: Value) -> Parameters {
    let mut parameters = Parameters::new();
    parameters.insert(name.to_owned(), value);
    parameters
}

fn at(longitude: f64, latitude: f64) -> Position {
    Position::new(longitude, latitude)
}

fn ring(corners: &[(f64, f64)]) -> Ring {
    let mut positions: Vec<Position> = corners
        .iter()
        .map(|&(longitude, latitude)| at(longitude, latitude))
        .collect();
    positions.push(positions[0]);
    Ring(positions)
}

/// Write `shape` into `places:1` as the field `where`, on a schemaless table.
fn write(session: &mut Session<'_>, shape: Geometry) -> Result<(), String> {
    write_to(session, 1, shape)
}

/// The same, into a record of the caller's choosing.
fn write_to(session: &mut Session<'_>, id: u32, shape: Geometry) -> Result<(), String> {
    session
        .run_with(
            &format!("CREATE places:{id} = {{ name: 'somewhere', where: $shape }};"),
            &bound("shape", Value::Geometry(shape)),
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The shape stored at `places:1`.
fn stored(session: &mut Session<'_>) -> Geometry {
    let outcomes = session.run("SELECT * FROM places;").unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a select answers with records")
    };
    let Value::Object(fields) = &records[0].1 else {
        panic!("a record is an object")
    };
    match fields.get("where") {
        Some(Value::Geometry(shape)) => shape.clone(),
        other => panic!("`where` should hold a shape, holds {other:?}"),
    }
}

// ----------------------------------------------- the gate runs at all

#[test]
fn a_shape_is_snapped_on_the_way_in_and_the_stored_one_is_the_snapped_one() {
    let store = store();
    let mut session = schemaless(&store);

    // Eleven decimals, two past the grid.
    let submitted = at(2.294_481_012_34, 48.858_370_987_65);
    write(&mut session, Geometry::Point(submitted)).expect("a point in Paris is a place");

    let Geometry::Point(kept) = stored(&mut session) else {
        panic!("a point stays a point")
    };
    assert_ne!(
        kept, submitted,
        "the store's grid is coarser than eleven decimals, so the value moved"
    );
    assert_eq!(kept.longitude, 2.294_481_012);
    assert_eq!(kept.latitude, 48.858_370_988);
}

#[test]
fn reading_and_writing_the_same_record_back_moves_nothing() {
    let store = store();
    let mut session = schemaless(&store);

    write(
        &mut session,
        Geometry::Point(at(2.294_481_012_34, 48.858_370_987_65)),
    )
    .unwrap();
    let once = stored(&mut session);

    // The cycle a caller performs without thinking about it: read a record,
    // change something else, write it back.
    session
        .run_with(
            "UPDATE places:1 = { name: 'somewhere else', where: $shape };",
            &bound("shape", Value::Geometry(once.clone())),
        )
        .unwrap();

    assert_eq!(
        stored(&mut session),
        once,
        "a second cycle must not move a vertex — this is what stops slow drift"
    );
}

#[test]
fn the_gate_fires_on_a_table_that_declares_nothing() {
    let store = store();
    let mut session = schemaless(&store);

    // No `DEFINE FIELD ... TYPE geometry` anywhere. The refusal proves the check
    // is about the value rather than about a declaration.
    let refusal = write(&mut session, Geometry::Point(at(181.0, 0.0)))
        .expect_err("181 is not a longitude, declared or not");
    assert!(refusal.contains("longitude"), "{refusal}");

    let outcomes = session.run("SELECT * FROM places;").unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a select answers with records")
    };
    assert!(
        records.is_empty(),
        "a refused write leaves nothing behind — not a partial record, not a null"
    );
}

// ------------------------------------------------- what it refuses

#[test]
fn a_longitude_off_the_sphere_is_refused_rather_than_wrapped() {
    let store = store();
    let mut session = schemaless(&store);
    let refusal = write(&mut session, Geometry::Point(at(181.0, 0.0))).unwrap_err();
    assert!(refusal.contains("longitude"), "{refusal}");
    assert!(refusal.contains("181"), "{refusal}");
}

#[test]
fn a_reversed_pair_is_caught_because_the_fixture_makes_it_catchable() {
    let store = store();
    let mut session = schemaless(&store);

    // 150 is a longitude and cannot be a latitude, so a layer that swapped the
    // pair fails here rather than storing a point in the wrong hemisphere with
    // nothing to report.
    write_to(&mut session, 1, Geometry::Point(at(150.0, 45.0)))
        .expect("longitude 150, latitude 45");
    let refusal = write_to(&mut session, 2, Geometry::Point(at(45.0, 150.0)))
        .expect_err("latitude 150 does not exist");
    assert!(refusal.contains("latitude"), "{refusal}");
}

#[test]
fn an_unclosed_ring_is_refused_and_the_message_says_where() {
    let store = store();
    let mut session = schemaless(&store);

    let open = Geometry::Polygon(Polygon {
        exterior: Ring(vec![at(0.0, 0.0), at(1.0, 0.0), at(1.0, 1.0), at(0.0, 1.0)]),
        interiors: Vec::new(),
    });
    let refusal = write(&mut session, open).unwrap_err();
    assert!(refusal.contains("end where it began"), "{refusal}");
    assert!(refusal.contains("shell"), "names the ring: {refusal}");
}

#[test]
fn a_hole_outside_its_shell_is_refused() {
    let store = store();
    let mut session = schemaless(&store);

    let elsewhere = Geometry::Polygon(Polygon {
        exterior: ring(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]),
        interiors: vec![ring(&[
            (20.0, 20.0),
            (20.0, 22.0),
            (22.0, 22.0),
            (22.0, 20.0),
        ])],
    });
    let refusal = write(&mut session, elsewhere).unwrap_err();
    assert!(refusal.contains("outside its shell"), "{refusal}");
}

#[test]
fn a_shape_the_grid_breaks_is_refused_even_though_it_arrived_well_formed() {
    let store = store();
    let mut session = schemaless(&store);

    let submitted = Geometry::Polygon(Polygon {
        exterior: ring(&[
            (0.0, 0.0),
            (1.0, 0.0),
            (1.0, 1.0),
            (0.000_000_000_2, 0.000_000_000_2),
        ]),
        interiors: Vec::new(),
    });
    assert!(
        submitted.is_well_formed(),
        "the precondition: it is a fine shape until the store rounds it"
    );

    let refusal = write(&mut session, submitted).unwrap_err();
    assert!(
        refusal.contains("same grid point"),
        "and the message explains that rounding is what did it: {refusal}"
    );
}

// ------------------------------------- everywhere a value can be

#[test]
fn a_shape_nested_in_an_array_in_a_field_is_reached_too() {
    let store = store();
    let mut session = schemaless(&store);

    let refusal = session
        .run_with(
            "CREATE places:1 = { route: [ { stop: $bad } ] };",
            &bound("bad", Value::Geometry(Geometry::Point(at(0.0, 91.0)))),
        )
        .expect_err("a shape two containers deep is still a shape");
    assert!(refusal.to_string().contains("latitude"), "{refusal}");
}

#[test]
fn a_key_value_write_goes_through_the_same_door() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE maps; USE NAMESPACE maps;\n\
             DEFINE DATABASE world; USE DATABASE world;\n\
             DEFINE SPACE marks;",
        )
        .unwrap();

    let refusal = session
        .run_with(
            "SET marks:'here' = $bad;",
            &bound("bad", Value::Geometry(Geometry::Point(at(0.0, 91.0)))),
        )
        .expect_err("`SET` replaces a value and is still a record write");
    assert!(refusal.to_string().contains("latitude"), "{refusal}");
}

#[test]
fn an_edge_carrying_a_shape_goes_through_it_as_well() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE maps; USE NAMESPACE maps;\n\
             DEFINE DATABASE world; USE DATABASE world;\n\
             DEFINE TABLE places;\n\
             DEFINE TABLE roads EDGE;\n\
             CREATE places:1 = { name: 'here' };\n\
             CREATE places:2 = { name: 'there' };",
        )
        .unwrap();

    let refusal = session
        .run_with(
            "RELATE places:1 -> roads -> places:2 = { via: $bad };",
            &bound("bad", Value::Geometry(Geometry::Point(at(0.0, 91.0)))),
        )
        .expect_err("an edge is an ordinary record and its properties are values");
    assert!(refusal.to_string().contains("latitude"), "{refusal}");
}

#[test]
fn an_ordinary_shape_is_stored_untouched() {
    let store = store();
    let mut session = schemaless(&store);

    // Every coordinate here is already a whole number of grid units, so the
    // boundary has nothing to do and must do nothing.
    let square = Geometry::Polygon(Polygon {
        exterior: ring(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]),
        interiors: vec![ring(&[(2.0, 2.0), (2.0, 4.0), (4.0, 4.0), (4.0, 2.0)])],
    });
    write(&mut session, square.clone()).expect("a square with a square hole is a polygon");
    assert_eq!(stored(&mut session), square);
}
