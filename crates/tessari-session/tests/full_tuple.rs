//! A condition that names both columns of an index, served as one lookup.
//!
//! `WHERE last = 'x' AND first = 'y'` against an index on `(last, first)` was
//! served through **`last` alone**, and the whole condition was then re-tested
//! above the source. Correct, and more work than the question deserves: one
//! surname may have any number of forenames, and `docs/tessariql.md` §8 recorded the
//! cost twice — once as the seek itself, and once as the reason a `UNIQUE`
//! composite could never promise a ceiling of one.
//!
//! The cause was not the storage layer. `Transaction::records_by_index` has
//! taken a slice of values since G003 and already turns a **complete** lookup on
//! a unique index into a point read. The cause was that the planner decided per
//! *clause*: it walked the conjuncts and, for each, handed one index a single
//! value. An index was never asked what the condition fixed for it.
//!
//! # What is asserted here, and why it is entries rather than milliseconds
//!
//! The counting backend reports the rows the scans and point reads handed back.
//! A read that answers the right one record having examined two hundred passes
//! every other test in this crate — the cost is invisible to all of them, and on
//! a table this size a timing would report it as noise.
//!
//! Every count is asserted as a **difference** against the same read narrowed by
//! the leading field only. Both resolve the same catalog before they read
//! anything, so subtracting cancels that constant and leaves an exact equality
//! about records; an assertion on the level would need a fudge factor and would
//! stop meaning anything the day the catalog changed shape.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value as KvValue,
    WriteBatch,
};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

/// How many records share one surname.
///
/// Large enough that examining all of them to answer with one is a difference no
/// fencepost could produce, small enough that every test here is instant.
const SHARED: u64 = 200;

/// A backend that answers exactly as the one beneath it and counts the rows it
/// handed back.
///
/// Rows rather than calls: a single scan that returned two hundred entries is
/// one call and a linear cost, so a call count would report this change as
/// having achieved nothing.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn KvBackend>,
    entries: AtomicUsize,
}

impl Counting {
    fn new(inner: Arc<dyn KvBackend>) -> Self {
        Self {
            inner,
            entries: AtomicUsize::new(0),
        }
    }

    fn entries(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }

    fn reset(&self) {
        self.entries.store(0, Ordering::Relaxed);
    }
}

impl KvBackend for Counting {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<KvValue>> {
        let found = self.inner.get(keyspace, key)?;
        if found.is_some() {
            self.entries.fetch_add(1, Ordering::Relaxed);
        }
        Ok(found)
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, KvValue)>> {
        let found = self.inner.scan(request)?;
        self.entries.fetch_add(found.len(), Ordering::Relaxed);
        Ok(found)
    }

    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, KvValue)>>> {
        let found = self.inner.first_of_each(keyspace, ranges)?;
        self.entries
            .fetch_add(found.iter().flatten().count(), Ordering::Relaxed);
        Ok(found)
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        self.inner.apply(batch)
    }
}

/// A store over a counting backend, and the counter beside it.
fn counted() -> (Store, Arc<Counting>) {
    let inner = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let counting = Arc::new(Counting::new(inner));
    let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
    (store, counting)
}

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// `SHARED` people who all share a surname, each with a distinct forename.
///
/// The surname is what a leading lookup finds; the forename is what tells them
/// apart. `city` and `town` are there for the ranking tests and are distinct per
/// record, so neither of them narrows less than the other.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION users;",
        )
        .unwrap();
    for n in 1..=SHARED {
        session
            .run(&format!(
                "CREATE users:{n} = {{ last: 'lovelace', first: 'n{n}', \
                 city: 'c{n}', town: 't{n}' }};"
            ))
            .unwrap();
    }
    session
}

const COMPOSITE: &str = "DEFINE INDEX by_name ON users FIELDS last, first;";
const UNIQUE: &str = "DEFINE INDEX by_name ON users FIELDS last, first UNIQUE;";
const LEADING: &str = "SELECT * FROM users WHERE last = 'lovelace';";
const TUPLE: &str = "SELECT * FROM users WHERE last = 'lovelace' AND first = 'n7';";

fn plan(session: &mut Session<'_>, read: &str, field: &str) -> String {
    let outcomes = session.run(&format!("EXPLAIN {read}")).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    format!("{:?}", fields.get(field).unwrap_or(&Value::None))
}

fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    let outcomes = session.run(read).unwrap();
    let mut found: Vec<RecordId> = outcomes
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

/// What one read cost the backend, with the counter reset around it.
fn entries(session: &mut Session<'_>, counting: &Counting, read: &str) -> usize {
    counting.reset();
    session.run(read).unwrap();
    counting.entries()
}

#[test]
fn a_condition_naming_both_columns_examines_one_record_instead_of_every_sharing_the_first() {
    let (store, counting) = counted();
    let mut session = ready(&store);
    session.run(COMPOSITE).unwrap();

    let leading = entries(&mut session, &counting, LEADING);
    let tuple = entries(&mut session, &counting, TUPLE);

    // The difference, not the level: both statements resolve the same catalog
    // before reading anything, and subtracting cancels it exactly.
    //
    // The leading read examines every record sharing the surname — one index
    // entry and one record read apiece, the second being the confirmation that
    // makes an index unable to change an answer. The tuple read examines one.
    // So the difference is two per record it no longer looks at.
    assert_eq!(
        leading.saturating_sub(tuple),
        usize::try_from(SHARED.saturating_sub(1)).unwrap() * 2,
        "leading {leading}, tuple {tuple}"
    );
    // …and the answer is the one record, which is what the count is about.
    assert_eq!(ids(&mut session, TUPLE), vec![RecordId::Int(7)]);
}

#[test]
fn the_plan_says_how_many_of_the_index_columns_the_lookup_fixes() {
    let store = store();
    let mut session = ready(&store);
    session.run(COMPOSITE).unwrap();

    assert_eq!(plan(&mut session, TUPLE, "index"), r#"String("by_name")"#);
    assert_eq!(plan(&mut session, TUPLE, "shape"), r#"String("equality")"#);
    // Equal to the index's arity is a complete lookup — the thing §8 recorded as
    // impossible to ask for.
    assert_eq!(plan(&mut session, TUPLE, "columns"), "Number(Integer(2))");
    // And fixing only the first is still fixing only the first.
    assert_eq!(plan(&mut session, LEADING, "columns"), "Number(Integer(1))");
}

#[test]
fn a_unique_composite_promises_one_exactly_when_the_condition_fixes_every_field() {
    // The ceiling §8 recorded as impossible without gathering by index, and the
    // observable proof that the gathering landed.
    let store = store();
    let mut session = ready(&store);
    session.run(UNIQUE).unwrap();

    assert_eq!(plan(&mut session, TUPLE, "at_most"), "Number(Integer(1))");
    // Unchanged where it was always true: fixing one field of a pair promises
    // nothing, because one surname may have any number of forenames — and
    // claiming a ceiling there would make the planner prefer an index that can
    // return the whole table.
    assert_eq!(plan(&mut session, LEADING, "at_most"), "None");
    assert_eq!(ids(&mut session, TUPLE), vec![RecordId::Int(7)]);
    assert_eq!(
        ids(&mut session, LEADING).len(),
        usize::try_from(SHARED).unwrap()
    );
}

#[test]
fn the_ceiling_is_what_makes_the_planner_choose_the_composite() {
    // C5's second half, and the one that would fail silently: a ceiling nothing
    // acts on is a number in a report. The competing index is declared *first*
    // and offers a shape that ranks identically, so only the ceiling can decide.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_last ON users FIELDS last;")
        .unwrap();
    session.run(UNIQUE).unwrap();

    assert_eq!(plan(&mut session, TUPLE, "index"), r#"String("by_name")"#);
    assert_eq!(plan(&mut session, TUPLE, "at_most"), "Number(Integer(1))");
    // Without a second column fixed there is no ceiling, so source order stands
    // and the index declared first wins.
    assert_eq!(plan(&mut session, LEADING, "index"), r#"String("by_last")"#);
    assert_eq!(ids(&mut session, TUPLE), vec![RecordId::Int(7)]);
}

#[test]
fn more_fixed_columns_wins_even_with_no_ceiling_to_show_for_it() {
    // The composite here is **not** unique, so both candidates report no
    // ceiling and the same shape. What separates them is a proof rather than an
    // estimate: the entries matching two fixed fields are a subset of those
    // matching the first alone, whatever the data holds.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_last ON users FIELDS last;")
        .unwrap();
    session.run(COMPOSITE).unwrap();

    assert_eq!(plan(&mut session, TUPLE, "index"), r#"String("by_name")"#);
    assert_eq!(plan(&mut session, TUPLE, "at_most"), "None");
    assert_eq!(plan(&mut session, TUPLE, "columns"), "Number(Integer(2))");
}

#[test]
fn a_tie_still_breaks_on_the_order_the_author_wrote() {
    // The property the restructuring could most easily have lost, and the only
    // one of these written down before this wave: gathering *by index* invites
    // an outer loop over indexes, which would make an equal-ranked tie resolve
    // by declaration order instead. `by_city` is declared first and `town` is
    // written first, so the two rules disagree and the answer says which one is
    // in force.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_city ON users FIELDS city;")
        .unwrap();
    session
        .run("DEFINE INDEX by_town ON users FIELDS town;")
        .unwrap();

    let read = "SELECT * FROM users WHERE town = 't3' AND city = 'c3';";
    assert_eq!(plan(&mut session, read, "index"), r#"String("by_town")"#);
    // Reversed in the condition and nothing else changes.
    let other = "SELECT * FROM users WHERE city = 'c3' AND town = 't3';";
    assert_eq!(plan(&mut session, other, "index"), r#"String("by_city")"#);
    assert_eq!(ids(&mut session, read), vec![RecordId::Int(3)]);
    assert_eq!(ids(&mut session, other), vec![RecordId::Int(3)]);
}

#[test]
fn a_mixed_tuple_fixes_only_the_equality_run() {
    // `records_by_index` takes exact values per field, so a prefix on the second
    // column cannot join the tuple. Gathering stops at the first field the
    // condition does not fix with an equality, and the condition re-tests the
    // rest — which it does to every candidate anyway.
    let store = store();
    let mut session = ready(&store);
    session.run(COMPOSITE).unwrap();

    let read = "SELECT * FROM users WHERE last = 'lovelace' AND first LIKE 'n7%';";
    assert_eq!(plan(&mut session, read, "index"), r#"String("by_name")"#);
    assert_eq!(plan(&mut session, read, "columns"), "Number(Integer(1))");
    // And the answer is still every record the condition matches, found by
    // re-testing rather than by the index.
    let mut expected = vec![RecordId::Int(7)];
    expected.extend(
        (70..=79)
            .filter(|n| *n <= SHARED)
            .map(|n| RecordId::Int(i64::try_from(n).unwrap())),
    );
    expected.sort();
    assert_eq!(ids(&mut session, read), expected);
}

#[test]
fn the_answers_are_the_ones_a_scan_gives() {
    // The rule the whole wave could have broken, over every configuration the
    // restructuring makes reachable — and the plans are asserted **different**,
    // so the equality is not several scans agreeing with each other.
    let reads = [
        LEADING,
        TUPLE,
        "SELECT * FROM users WHERE last = 'lovelace' AND first = 'nobody';",
        "SELECT * FROM users WHERE first = 'n7' AND last = 'lovelace';",
        "SELECT * FROM users WHERE last = 'nobody' AND first = 'n7';",
        "SELECT * FROM users WHERE last = 'lovelace' AND first = 'n7' AND city = 'c7';",
        "SELECT * FROM users WHERE city = 'c7' AND last = 'lovelace' AND first = 'n7';",
    ];
    let plain = store();
    let mut without = ready(&plain);
    let indexed = store();
    let mut with = ready(&indexed);
    with.run("DEFINE INDEX by_last ON users FIELDS last;")
        .unwrap();
    with.run(COMPOSITE).unwrap();

    for read in reads {
        assert_eq!(
            plan(&mut without, read, "access"),
            r#"String("scan")"#,
            "{read}"
        );
        assert_eq!(
            plan(&mut with, read, "access"),
            r#"String("index")"#,
            "{read}"
        );
        assert_eq!(ids(&mut with, read), ids(&mut without, read), "{read}");
    }
    // …and one expected answer written out, so the equality above is not two
    // wrong answers agreeing.
    assert_eq!(ids(&mut with, TUPLE), vec![RecordId::Int(7)]);
}
