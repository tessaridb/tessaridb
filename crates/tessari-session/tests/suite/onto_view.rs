//! A grouped read answered from a Grouped materialized view's stored rows
//! (G102 S12 v1): when the rewrite's conditions all hold the read is served
//! from the view and says so with a note, and its answer is byte-identical
//! to the same read over a table no view covers — the differential oracle,
//! with the note as the control arm that proves the rewrite took effect.
//!
//! Every test here comes in two halves that must agree: the twin comparison
//! pins correctness, and the note pins that the comparison exercised the
//! rewritten path rather than two base-table reads agreeing for the wrong
//! reason.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use tessari_session::{Now, Outcome, RunNow, Session, maintain_views};
use tessari_storage::Store;
use tessari_types::{Number, Value};

use super::materialized_views::{ready, store};

/// The note kind a rewritten answer carries.
const ANSWERED_BY_VIEW: &str = "answered-by-view";

/// The grouped view most tests here answer from.
const BANDS: &str = "SELECT band, count(*) AS orders, sum(total) AS spent, \
                     min(total) AS low, max(total) AS high FROM orders GROUP BY band";

/// An identical `plain` collection no view covers, beside `ready`'s `orders`,
/// with the grouped view defined over `orders` and maintained to the head.
fn twinned(store: &Store) -> Session<'_> {
    let mut session = ready(store);
    session.run_now("DEFINE COLLECTION plain;").unwrap();
    let mut script = String::new();
    for n in 0..40_i64 {
        script.push_str(&format!(
            "CREATE plain:{n} = {{ band: {}, total: {}, n: {n} }};\n",
            n % 4,
            n.wrapping_mul(7) % 50
        ));
    }
    session.run_now(&script).unwrap();
    session
        .run_now(&format!("DEFINE VIEW bands MATERIALIZED AS {BANDS};"))
        .unwrap();
    maintain_views(store).now().unwrap();
    session
}

/// Run one read over both tables; answer `(viewed, plain, rewritten?)`.
fn both(session: &mut Session<'_>, read: &str) -> (Vec<Value>, Vec<Value>, bool) {
    let run = |session: &mut Session<'_>, table: &str| {
        let outcomes = session.run_now(&read.replace("{t}", table)).unwrap();
        let last = outcomes.last().unwrap();
        let rows = last
            .records()
            .unwrap()
            .iter()
            .map(|(_, value)| value.clone())
            .collect::<Vec<_>>();
        let rewritten = last
            .notes()
            .iter()
            .any(|note| note.kind() == ANSWERED_BY_VIEW);
        (rows, rewritten)
    };
    let (viewed, rewritten) = run(session, "orders");
    let (plain, plain_rewritten) = run(session, "plain");
    assert!(!plain_rewritten, "no view covers `plain`");
    (viewed, plain, rewritten)
}

#[test]
fn a_covered_read_is_answered_from_the_kept_view() {
    let store = store();
    let mut session = twinned(&store);
    let (viewed, plain, rewritten) = both(
        &mut session,
        "SELECT band, sum(total) AS spent FROM {t} GROUP BY band;",
    );
    assert!(rewritten, "the read was not answered from the view");
    assert_eq!(viewed, plain, "the rewritten answer diverged");
    // Pinned to content, not only to agreement: band 0 holds rows 0,4,…,36 —
    // ten rows of totals (0, 28, 6, 34, 12, 40, 18, 46, 24, 2).
    let expected: i64 = [0, 28, 6, 34, 12, 40, 18, 46, 24, 2].iter().sum();
    let Some(Value::Object(first)) = viewed.first() else {
        panic!("no first row: {viewed:?}");
    };
    assert_eq!(
        first.get("spent"),
        Some(&Value::Number(Number::Integer(expected))),
    );
}

#[test]
fn a_subset_grouping_and_a_grand_aggregate_refold() {
    let store = store();
    let mut session = twinned(&store);
    // The view groups by band alone, so a read of one of its folds with the
    // same grouping re-folds trivially, and the grand aggregate folds every
    // group into one: a count of counts, a sum of sums.
    for read in [
        "SELECT sum(total) AS spent FROM {t} GROUP BY band;",
        "SELECT count(*) AS n FROM {t};",
        "SELECT count(*) AS n, min(total) AS low, max(total) AS high FROM {t};",
    ] {
        let (viewed, plain, rewritten) = both(&mut session, read);
        assert!(rewritten, "`{read}` was not answered from the view");
        assert_eq!(viewed, plain, "`{read}` diverged");
    }
    // Pinned to content: 40 rows, totals (n*7)%50.
    let (_, plain, _) = both(&mut session, "SELECT count(*) AS n FROM {t};");
    let Some(Value::Object(row)) = plain.first() else {
        panic!("no row: {plain:?}");
    };
    assert_eq!(row.get("n"), Some(&Value::Number(Number::Integer(40))),);
}

#[test]
fn an_extra_conjunct_over_a_group_key_keeps_whole_groups() {
    let store = store();
    let mut session = twinned(&store);
    // `band > 1` is constant within each of the view's groups, so it keeps
    // and drops whole groups and the re-fold stays exact.
    let (viewed, plain, rewritten) = both(
        &mut session,
        "SELECT band, sum(total) AS spent FROM {t} WHERE band > 1 GROUP BY band;",
    );
    assert!(rewritten, "the read was not answered from the view");
    assert_eq!(viewed, plain, "the rewritten answer diverged");
    assert_eq!(viewed.len(), 2, "bands 2 and 3 remain");
}

#[test]
fn ordering_and_a_bound_ride_the_rewrite() {
    let store = store();
    let mut session = twinned(&store);
    let (viewed, plain, rewritten) = both(
        &mut session,
        "SELECT band, sum(total) AS spent FROM {t} GROUP BY band ORDER BY spent DESC LIMIT 2;",
    );
    assert!(rewritten, "the read was not answered from the view");
    assert_eq!(viewed, plain, "the rewritten answer diverged");
    assert_eq!(viewed.len(), 2);
}

#[test]
fn what_the_rewrite_does_not_cover_is_not_rewritten() {
    let store = store();
    let mut session = twinned(&store);
    // Each of these is answered correctly by the base table, and the note
    // says the view did not answer it: a fold the view does not hold, a
    // conjunct that is not group-constant, and a grouping coarser than any
    // the view can re-fold into is fine but a fold over a DIFFERENT
    // expression is not.
    for read in [
        "SELECT band, mean(total) AS avg FROM {t} GROUP BY band;",
        "SELECT band, sum(n) AS total_n FROM {t} GROUP BY band;",
        "SELECT band, sum(total) AS spent FROM {t} WHERE n > 10 GROUP BY band;",
        "SELECT n, sum(total) AS spent FROM {t} GROUP BY n;",
    ] {
        let (viewed, plain, rewritten) = both(&mut session, read);
        assert!(!rewritten, "`{read}` must not be answered from the view");
        assert_eq!(viewed, plain, "`{read}` diverged");
    }
}

#[test]
fn a_stale_view_does_not_answer() {
    let store = store();
    let mut session = twinned(&store);
    // A write the view has not been maintained past: the rewrite declines,
    // the base table answers, and the answer counts the new row.
    session
        .run_now("CREATE orders:100 = { band: 1, total: 5, n: 100 };")
        .unwrap();
    let outcomes = session
        .run_now("SELECT count(*) AS n FROM orders;")
        .unwrap();
    let last = outcomes.last().unwrap();
    assert!(
        !last
            .notes()
            .iter()
            .any(|note| note.kind() == ANSWERED_BY_VIEW),
        "a stale view answered"
    );
    let rows = last.records().unwrap();
    let Some((_, Value::Object(row))) = rows.first() else {
        panic!("no row: {rows:?}");
    };
    assert_eq!(row.get("n"), Some(&Value::Number(Number::Integer(41))));
    // Maintained, the same read is the view's again.
    maintain_views(&store).now().unwrap();
    let outcomes = session
        .run_now("SELECT count(*) AS n FROM orders;")
        .unwrap();
    let last = outcomes.last().unwrap();
    assert!(
        last.notes()
            .iter()
            .any(|note| note.kind() == ANSWERED_BY_VIEW),
        "a current view did not answer"
    );
}

#[test]
fn a_narrowed_grant_is_never_answered_from_the_view() {
    let store = store();
    let mut session = twinned(&store);
    session
        .run_now("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", "correct horse battery").now().unwrap();
    root.run_now(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
         DEFINE USER narrow ON NAMESPACE prod AUTHORITIES read \
         PASSWORD 'correct horse battery';\n\
         DEFINE USER whole ON NAMESPACE prod AUTHORITIES read \
         PASSWORD 'correct horse battery';\n\
         GRANT read ON orders FIELDS band TO narrow;\n\
         GRANT read ON orders TO whole;",
    )
    .unwrap();
    let read = "SELECT band, sum(total) AS spent FROM orders GROUP BY band;";
    // The stored rows were computed for everybody: a caller whose grant
    // hides `total` must not be answered from them — the view would disclose
    // what the grant hides — so the rewrite declines and the base read
    // answers as it always has for this caller.
    let mut narrow = Session::new(&store);
    narrow
        .sign_in("narrow", "correct horse battery")
        .now()
        .unwrap();
    narrow
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let outcomes = narrow.run_now(read).unwrap();
    let last = outcomes.last().unwrap();
    assert!(
        !last
            .notes()
            .iter()
            .any(|note| note.kind() == ANSWERED_BY_VIEW),
        "a field-narrowed caller was answered from the view"
    );
    // A grant naming no fields is no restriction, and the rewrite serves it.
    let mut whole = Session::new(&store);
    whole
        .sign_in("whole", "correct horse battery")
        .now()
        .unwrap();
    whole
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    let outcomes = whole.run_now(read).unwrap();
    let last = outcomes.last().unwrap();
    assert!(
        last.notes()
            .iter()
            .any(|note| note.kind() == ANSWERED_BY_VIEW),
        "an unrestricted caller was not answered from the view"
    );
}

#[test]
fn explain_names_the_view_the_read_would_take() {
    let store = store();
    let mut session = twinned(&store);
    let outcomes = session
        .run_now("EXPLAIN SELECT band, sum(total) AS spent FROM orders GROUP BY band;")
        .unwrap();
    let Some(Outcome::Value(Value::Object(plan))) = outcomes.last() else {
        panic!("no plan: {outcomes:?}");
    };
    assert_eq!(
        plan.get("answered-by-view"),
        Some(&Value::from("bands")),
        "the plan did not name the view: {plan:?}"
    );
    // A read the rewrite declines plans as the base-table read it is.
    let outcomes = session
        .run_now("EXPLAIN SELECT band, mean(total) AS avg FROM orders GROUP BY band;")
        .unwrap();
    let Some(Outcome::Value(Value::Object(plan))) = outcomes.last() else {
        panic!("no plan: {outcomes:?}");
    };
    assert!(
        plan.get("answered-by-view").is_none(),
        "an uncovered read named a view: {plan:?}"
    );
}

#[test]
fn an_empty_source_folds_the_same_both_ways() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE COLLECTION bare; DEFINE COLLECTION bare_plain;")
        .unwrap();
    session
        .run_now("DEFINE VIEW bare_bands MATERIALIZED AS SELECT band, count(*) AS orders, sum(total) AS spent, min(total) AS low FROM bare GROUP BY band;")
        .unwrap();
    maintain_views(&store).now().unwrap();
    // Whatever shape a grand aggregate over nothing has in this engine —
    // the rewrite must answer it exactly, from a view that holds no rows.
    let read = "SELECT count(*) AS n, sum(total) AS spent, min(total) AS low FROM ";
    let viewed = session
        .run_now(&format!("{read}bare;"))
        .unwrap()
        .pop()
        .unwrap();
    let plain = session
        .run_now(&format!("{read}bare_plain;"))
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        viewed
            .notes()
            .iter()
            .any(|note| note.kind() == ANSWERED_BY_VIEW),
        "the empty view did not answer"
    );
    assert_eq!(
        viewed.records().unwrap(),
        plain.records().unwrap(),
        "the empty re-fold diverged from the base read"
    );
}
