//! Undeclaring a catalog object, and the four refusals that make it safe.
//!
//! # What this wave was for
//!
//! The language could declare twelve kinds of thing and undeclare six. That is
//! not a missing-feature list — it is a store reachable into a shape no
//! statement gets it out of, whose only exit was editing the catalog by hand.
//!
//! # The property the corpus cannot assert, and this file can
//!
//! Three of these refusals are about **what is still pointing at the thing**,
//! and the interesting half of each is not that it fires. It is:
//!
//! - that the refusal **names the dependant**, so acting on it needs no second
//!   query — a message reading only *not empty* leaves the reader to go and run
//!   the query this statement already ran;
//! - that it **writes nothing**, so a refused drop leaves the object usable
//!   rather than half-removed;
//! - and that it **stops firing** once the dependant is gone, which is the half
//!   that would otherwise make the rule worse than the hole it fills.
//!
//! The corpus asserts the variant name. Only a test can read the message.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn opened(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run(SCHEMA).unwrap();
    session
}

fn ok(session: &mut Session<'_>, script: &str) {
    session
        .run(script)
        .unwrap_or_else(|error| panic!("{script}: {error}"));
}

/// The refusal, as a caller reads it.
fn refusal(session: &mut Session<'_>, script: &str) -> String {
    session
        .run(script)
        .err()
        .map(|error| error.to_string())
        .unwrap_or_else(|| panic!("{script}: answered instead of refusing"))
}

/// The refusal carries the field, not just the fact that there was one.
///
/// A field attaches an analyzer **by name**, so nothing in the catalog enforces
/// the link. Removing the analyzer would leave `posts.body` naming something
/// that no longer resolves, and the symptom of that is a search which quietly
/// stops matching — a wrong answer indistinguishable from a right one.
#[test]
fn dropping_an_analyzer_a_field_names_refuses_and_says_which_field() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE ANALYZER simple FILTERS lowercase;
         DEFINE TABLE posts SCHEMALESS;
         DEFINE FIELD body ON posts TYPE string ANALYZER simple;",
    );
    let message = refusal(&mut session, "DROP ANALYZER simple;");
    assert!(
        message.contains("body"),
        "does not name the field: {message}"
    );
    assert!(message.contains("simple"), "does not name it: {message}");
    assert!(
        message.contains("CASCADE"),
        "does not say why there is no cascade: {message}"
    );
}

/// A refused drop leaves the analyzer usable, not half-removed.
///
/// The refusal happens before the catalog write, but that is an implementation
/// detail; what a caller is owed is that the failed statement changed nothing,
/// and the only way to see it is to use the object afterwards.
#[test]
fn a_refused_analyzer_drop_leaves_it_attachable_to_another_field() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE ANALYZER simple FILTERS lowercase;
         DEFINE TABLE posts SCHEMALESS;
         DEFINE FIELD body ON posts TYPE string ANALYZER simple;",
    );
    refusal(&mut session, "DROP ANALYZER simple;");
    ok(
        &mut session,
        "DEFINE FIELD title ON posts TYPE string ANALYZER simple;",
    );
}

/// And it stops refusing, which is the half that makes the rule usable.
#[test]
fn an_analyzer_goes_once_the_last_field_naming_it_is_gone() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE ANALYZER simple FILTERS lowercase;
         DEFINE TABLE posts SCHEMALESS;
         DEFINE FIELD body ON posts TYPE string ANALYZER simple;
         DEFINE FIELD title ON posts TYPE string ANALYZER simple;",
    );
    // One of the two is not enough, and the count says so rather than the
    // refusal simply repeating itself.
    ok(&mut session, "DROP FIELD body ON posts;");
    let message = refusal(&mut session, "DROP ANALYZER simple;");
    assert!(
        message.contains("title"),
        "names the wrong field: {message}"
    );
    ok(&mut session, "DROP FIELD title ON posts;");
    ok(&mut session, "DROP ANALYZER simple;");
}

/// A field that names no analyzer holds nothing back.
///
/// The half that would make this rule worse than the hole: if the check asked
/// whether any field existed rather than whether any field named *this*
/// analyzer, every store with a schema would be unable to remove one.
#[test]
fn a_field_that_names_a_different_analyzer_does_not_hold_this_one() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE ANALYZER simple FILTERS lowercase;
         DEFINE ANALYZER folded FILTERS lowercase, ascii;
         DEFINE TABLE posts SCHEMALESS;
         DEFINE FIELD body ON posts TYPE string ANALYZER folded;
         DEFINE FIELD plain ON posts TYPE string;",
    );
    ok(&mut session, "DROP ANALYZER simple;");
}

/// The tenancy refusals count what they found and name the first.
#[test]
fn dropping_a_database_that_holds_tables_refuses_and_counts_them() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE COLLECTION orders; DEFINE COLLECTION invoices;",
    );
    let message = refusal(&mut session, "DROP DATABASE shop;");
    assert!(message.contains('2'), "does not count them: {message}");
    assert!(message.contains("tables"), "does not say what: {message}");
    assert!(
        message.contains("orders") || message.contains("invoices"),
        "does not name one: {message}"
    );
}

#[test]
fn an_empty_database_goes_and_a_namespace_follows_once_it_is_the_last() {
    let store = store();
    let mut session = opened(&store);
    // `prod` holds `shop`, so the namespace is refused first.
    let message = refusal(&mut session, "DROP NAMESPACE prod;");
    assert!(message.contains("shop"), "does not name it: {message}");
    assert!(message.contains("databases"), "wrong noun: {message}");
    ok(&mut session, "DROP DATABASE shop;");
    ok(&mut session, "DROP NAMESPACE prod;");
}

/// Tightening a populated table is refused while a row does not fit.
///
/// This is the same stance `DEFINE FIELD` takes over data that already violates
/// it, and the check runs where the store's own schema pass runs rather than in
/// a scan the executor writes — the comment in `schema.rs` named this statement
/// as the reason that pass would one day need a second trigger.
#[test]
fn tightening_a_table_is_refused_while_a_stored_row_carries_an_undeclared_field() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE TABLE notes SCHEMALESS;
         CREATE notes:1 = { title: 'first', extra: 'whatever' };
         DEFINE FIELD title ON notes TYPE string;",
    );
    let message = refusal(&mut session, "ALTER TABLE notes SET SCHEMAFULL;");
    assert!(message.contains("extra"), "does not name it: {message}");
    // Nothing was written, so the table is still schemaless and still takes it.
    ok(
        &mut session,
        "CREATE notes:2 = { title: 'second', extra: 'again' };",
    );
    // Declare what the rows carry, and it goes through.
    ok(
        &mut session,
        "DEFINE FIELD extra ON notes TYPE string;
         ALTER TABLE notes SET SCHEMAFULL;",
    );
    session
        .run("CREATE notes:3 = { title: 'third', beyond: 'no' };")
        .expect_err("a schemafull table took an undeclared field");
}

/// Widening is never refused, because no stored row can contradict it.
#[test]
fn loosening_a_table_is_never_refused() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE TABLE notes SCHEMALESS;
         DEFINE FIELD title ON notes TYPE string;
         ALTER TABLE notes SET SCHEMAFULL;",
    );
    session
        .run("CREATE notes:1 = { title: 'first', extra: 'no' };")
        .expect_err("a schemafull table took an undeclared field");
    ok(&mut session, "ALTER TABLE notes SET SCHEMALESS;");
    ok(
        &mut session,
        "CREATE notes:1 = { title: 'first', extra: 'yes' };",
    );
}

/// A bucket takes its chunk table with it.
///
/// The chunk table's name carries a byte no identifier can hold, so no statement
/// can ever name it — which makes an orphaned one permanent. The symptom is that
/// redefining the bucket fails on a name nobody can see.
#[test]
fn a_dropped_bucket_can_be_defined_again() {
    let store = store();
    let mut session = opened(&store);
    ok(&mut session, "DEFINE BUCKET media;");
    ok(&mut session, "DROP BUCKET media;");
    ok(&mut session, "DEFINE BUCKET media;");
    // And by the other spelling, because a bucket is a table.
    ok(&mut session, "DROP TABLE media;");
    ok(&mut session, "DEFINE BUCKET media;");
}

/// `DROP NODE` is declined rather than missing, and the message does the work.
///
/// A refusal that only said *unexpected token* would leave a reader who guessed
/// the obvious spelling with nowhere to go. This one names the configuration as
/// the place to change and `DROP REPLICA` as the statement for the related job.
#[test]
fn dropping_a_node_is_declined_and_points_at_what_to_do_instead() {
    let store = store();
    let mut session = opened(&store);
    let message = refusal(&mut session, "DROP NODE second;");
    assert!(
        message.contains("configuration"),
        "does not say where the change belongs: {message}"
    );
    assert!(
        message.contains("DROP REPLICA"),
        "does not point at the related statement: {message}"
    );
}

/// A peer is declared and undeclared, and undeclaring an absent one refuses.
#[test]
fn a_replica_goes_and_going_twice_is_refused() {
    let store = store();
    let mut session = opened(&store);
    ok(&mut session, "DEFINE REPLICA warsaw AT 'warsaw:9001';");
    ok(&mut session, "DROP REPLICA warsaw;");
    let message = refusal(&mut session, "DROP REPLICA warsaw;");
    assert!(message.contains("replica"), "wrong noun: {message}");
    assert!(message.contains("warsaw"), "does not name it: {message}");
}

/// `ALTER FIELD` replaces the declaration whole, and the rows answer for it.
///
/// The property the corpus cannot see: that the refusal leaves **neither** half
/// applied. A drop and a declaration in one commit could fail after removing the
/// old one, and the store would answer every subsequent write as though the
/// field had never been declared — which looks like success.
#[test]
fn a_refused_field_alteration_leaves_the_old_declaration_standing() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE COLLECTION people;
         ALTER TABLE people ADD FIELD name TYPE string;
         CREATE people:1 = { name: 'ada' };",
    );
    let message = refusal(
        &mut session,
        "ALTER TABLE people ALTER FIELD name TYPE int;",
    );
    assert!(
        message.contains("name"),
        "does not name the field: {message}"
    );
    // The old declaration is still enforcing: an int is still refused, which it
    // would not be if the drop had landed and the redeclaration had not.
    session
        .run("CREATE people:2 = { name: 42 };")
        .expect_err("the dropped half of a refused alteration was applied");
    ok(&mut session, "CREATE people:3 = { name: 'grace' };");
}

/// The two spellings parse the same declaration, option for option.
///
/// Written as a test rather than trusted to the shared function, because the
/// sharing is the thing that could be undone by a later edit — and the symptom
/// would be one spelling silently ignoring a `DEFAULT` somebody wrote.
#[test]
fn the_columnar_spelling_takes_every_option_the_long_one_does() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE COLLECTION people;
         ALTER TABLE people ADD FIELD name TYPE string REQUIRED;
         ALTER TABLE people ADD FIELD rank TYPE string DEFAULT 'viewer';
         ALTER TABLE people ADD FIELD bio TYPE string;",
    );
    ok(&mut session, "CREATE people:1 = { name: 'ada' };");
    // The default was taken, so the columnar spelling did not drop the clause.
    let outcome = session
        .run("RETURN (SELECT rank FROM people:1);")
        .unwrap()
        .pop()
        .expect("one outcome");
    assert!(
        format!("{outcome:?}").contains("viewer"),
        "the DEFAULT was parsed and then ignored: {outcome:?}"
    );
    session
        .run("CREATE people:2 = { rank: 'editor' };")
        .expect_err("REQUIRED was parsed and then ignored");
}

/// One statement declares the table and its fields.
///
/// The property is not that it parses. It is that what comes out is
/// **indistinguishable** from the long spelling: the executor desugars into the
/// same `define_field`, so a column is a field in every way a field is one, and
/// there is no second set of rules for the shorter form to disagree under.
#[test]
fn a_table_and_its_columns_are_one_statement() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE TABLE people (name string REQUIRED, rank string DEFAULT 'viewer');",
    );
    ok(&mut session, "CREATE people:1 = { name: 'ada' };");

    let outcome = session
        .run("RETURN (SELECT rank FROM people:1);")
        .unwrap()
        .pop()
        .expect("one outcome");
    assert!(
        format!("{outcome:?}").contains("viewer"),
        "the column's DEFAULT did not reach the field: {outcome:?}"
    );
    session
        .run("CREATE people:2 = { rank: 'editor' };")
        .expect_err("the column's REQUIRED did not reach the field");
}

/// Declaring columns makes the table refuse the ones it does not name.
///
/// The reading was the other way round until G018 node T: parentheses declared
/// fields and constrained nothing else, so the mistake worth catching — a
/// misspelled field name — wrote a new field and reported success. Both
/// spellings survive the change: `SCHEMAFULL` still says what is now already
/// true, and `SCHEMALESS` is the one word that buys the older reading back.
#[test]
fn columns_imply_strictness_and_schemaless_is_the_way_back() {
    let store = store();
    let mut session = opened(&store);

    ok(&mut session, "DEFINE TABLE tight (name string);");
    let refused = refusal(&mut session, "CREATE tight:1 = { name: 'ada', extra: 1 };");
    assert!(
        refused.contains("extra"),
        "the refusal should name the undeclared field: {refused}"
    );

    ok(
        &mut session,
        "DEFINE TABLE stated (name string) SCHEMAFULL;",
    );
    refusal(&mut session, "CREATE stated:1 = { name: 'ada', extra: 1 };");

    ok(&mut session, "DEFINE TABLE loose (name string) SCHEMALESS;");
    ok(&mut session, "CREATE loose:1 = { name: 'ada', extra: 1 };");
}

/// A column's constraint is checked against the rows already in the table.
///
/// The half of this criterion that is not about grammar. A declaration accepted
/// over data that contradicts it is worse than one refused: the catalog then
/// says something about the table that the table does not do, and every reader
/// downstream believes it.
#[test]
fn a_column_declared_over_violating_rows_is_refused_and_writes_nothing() {
    let store = store();
    let mut session = opened(&store);
    ok(&mut session, "DEFINE COLLECTION readings;");
    ok(&mut session, "CREATE readings:1 = { level: 'high' };");

    let refused = refusal(
        &mut session,
        "DEFINE TABLE IF NOT EXISTS readings (level int);",
    );
    // Named, so the refusal is the schema check over the stored row and not
    // `IF NOT EXISTS` declining the whole statement — which would make this
    // test pass while proving nothing.
    assert!(
        refused.contains("level"),
        "refused for some other reason than the column: {refused}"
    );
    // Nothing was written: the field is still undeclared, so a row that would
    // violate it is still accepted.
    ok(&mut session, "CREATE readings:2 = { level: 'low' };");
}

/// A refusal at the third column takes the first two, and the table, with it.
///
/// The statement's transaction is the unit. A half-declared table is the state
/// this must never leave behind, because the reader who wrote one statement has
/// no reason to go looking for a partial one.
#[test]
fn a_failing_column_rolls_back_the_columns_before_it_and_the_table() {
    let store = store();
    let mut session = opened(&store);
    refusal(
        &mut session,
        "DEFINE TABLE half (first string, second int, first bool);",
    );
    session
        .run("SELECT * FROM half;")
        .expect_err("the table survived a refused declaration");
}

/// Empty parentheses are a list somebody meant to fill in.
///
/// The flag-only spelling already says *no columns* by writing nothing, so `()`
/// carries no reading of its own — and accepting it silently would make a
/// truncated statement look like a deliberate one.
#[test]
fn empty_parentheses_are_refused_rather_than_read_as_no_columns() {
    let store = store();
    let mut session = opened(&store);
    refusal(&mut session, "DEFINE TABLE nothing ();");
}

/// A declared type says what it holds, and the schema report says it back.
///
/// `TYPE string` is true about a status column and says nothing. An `ASSERT`
/// says the right thing in the wrong place: a reader of the schema does not see
/// it, and a reader of the refusal gets a condition instead of a list. This is
/// the declaration that puts the set where both of them look.
#[test]
fn a_union_of_literals_is_a_declared_type_and_reads_back_as_one() {
    let store = store();
    let mut session = opened(&store);
    ok(
        &mut session,
        "DEFINE TABLE posts (status 'draft' | 'published' | 'archived');",
    );

    ok(&mut session, "CREATE posts:1 = { status: 'draft' };");
    let refused = refusal(&mut session, "CREATE posts:2 = { status: 'deleted' };");
    assert!(
        refused.contains("'archived' | 'draft' | 'published'"),
        "the refusal should name the set the field may hold: {refused}"
    );

    // Read back out of the catalog through the report a caller actually uses.
    // This is the half that makes it a *declared type* rather than a check: the
    // schema says what the field holds, so a reader learns it without running a
    // write and being told off.
    let reported = format!(
        "{:?}",
        session
            .run("INFO FOR TABLE posts;")
            .unwrap()
            .pop()
            .expect("one outcome")
    );
    assert!(
        reported.contains("'archived' | 'draft' | 'published'"),
        "the schema report did not carry the declared type: {reported}"
    );
}

/// The set is a set: two spellings of one union are one type.
#[test]
fn a_union_declared_in_another_order_is_the_same_declaration() {
    let store = store();
    let mut session = opened(&store);
    ok(&mut session, "DEFINE TABLE a (s 'x' | 'y');");
    ok(&mut session, "DEFINE TABLE b (s 'y' | 'x' | 'y');");
    let one = format!("{:?}", session.run("INFO FOR TABLE a;").unwrap());
    let other = format!("{:?}", session.run("INFO FOR TABLE b;").unwrap());
    assert!(one.contains("'x' | 'y'"), "{one}");
    assert!(other.contains("'x' | 'y'"), "{other}");
}

/// A default is checked against the union when the field is declared.
///
/// The same symmetry the rest of the schema follows: a declaration is checked
/// when it is made, not when it first bites. Without it the catalog holds a
/// default no write of that field could ever accept, and the failure arrives
/// later looking like the write's fault.
#[test]
fn a_default_outside_the_union_is_refused_at_declaration() {
    let store = store();
    let mut session = opened(&store);
    refusal(
        &mut session,
        "DEFINE TABLE posts (status 'draft' | 'published' DEFAULT 'deleted');",
    );
    ok(
        &mut session,
        "DEFINE TABLE posts (status 'draft' | 'published' DEFAULT 'draft');",
    );
}
