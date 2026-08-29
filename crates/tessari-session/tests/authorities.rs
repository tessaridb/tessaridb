//! The four rules the authority model was asked for, each as a script that runs.
//!
//! The file has two halves and they were written a wave apart. The first
//! asserts that each rule can be **said** and **read back** — that the store
//! holds the set the statement described and no more. The second, below the
//! divider, asserts what a held set causes the store to **refuse**, which is
//! what makes this a permission system rather than a vocabulary.
//!
//! Keeping both matters: a rule that can be said and is not enforced is a
//! promise the store does not keep, and a rule that is enforced and cannot be
//! said is one nobody can ask for.
//!
//! The rules, in the words they were given in:
//!
//! 1. the highest authority runs the server and the cluster;
//! 2. cluster management can stand on its own;
//! 3. a namespace has an authority that creates and drops databases in it;
//! 4. reading or writing inside a namespace confers **neither** of those.
//!
//! The fourth is the one no ladder could express, and it is the reason the model
//! is a set of `(kind, reach)` pairs rather than a rank: `write` and `manage`
//! have to be independent in both directions, and in a total order they cannot
//! be — put `manage` above `write` and every manager writes, put it below and
//! every writer manages.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Two namespaces, a database in each, and a store owner to declare the rest.
///
/// Two namespaces rather than one so that a reach can be *wrong* rather than
/// merely absent: an authority over `prod` that also answered for `staging`
/// would pass every single-namespace test ever written.
fn governed(store: &Store) {
    let mut opening = Session::new(store);
    opening
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders;\n\
             DEFINE NAMESPACE staging; USE NAMESPACE staging;\n\
             DEFINE DATABASE sandbox;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
}

/// A signed-in session, with no tenancy selected.
fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
}

/// What `INFO FOR USER` says a user holds, as `kind@reach` strings.
///
/// Read through the statement rather than out of the catalog on purpose: a test
/// that reached past the report would pass over a grant nobody could see, and
/// the report was the *only* observable effect a grant had for the wave between
/// the vocabulary landing and the enforcement below it.
fn held(session: &mut Session<'_>, user: &str) -> Vec<String> {
    let outcomes = session.run(&format!("INFO FOR USER {user};")).unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("expected a report, got {outcomes:?}");
    };
    let Some(Value::Array(authorities)) = report.get("authorities") else {
        panic!("expected an authority list, got {report:?}");
    };
    let mut written: Vec<String> = authorities
        .iter()
        .map(|held| {
            let Value::Object(one) = held else {
                panic!("expected an object per authority");
            };
            match (one.get("authority"), one.get("reach")) {
                (Some(Value::String(kind)), Some(Value::String(reach))) => {
                    format!("{kind}@{reach}")
                }
                other => panic!("expected a kind at a reach, found {other:?}"),
            }
        })
        .collect();
    written.sort();
    written
}

#[test]
fn the_highest_authority_holds_every_kind_over_the_whole_store() {
    // Rule 1. And the shape of the answer matters as much as its content: the
    // top is five ordinary authorities at store reach, not an `is_root` branch.
    // A privileged branch is how a model acquires a path its negative tests
    // never cover, because there is nothing there to write a negative test
    // against.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    assert_eq!(
        held(&mut root, "root"),
        vec![
            "govern@store".to_owned(),
            "manage@store".to_owned(),
            "operate@store".to_owned(),
            "read@store".to_owned(),
            "write@store".to_owned(),
        ]
    );
}

#[test]
fn running_the_cluster_can_stand_alone() {
    // Rule 2. `operate` over the store and nothing else: this user is trusted
    // with topology, replicas and the backup file, and with none of the data
    // those things move around. No role names this set — `owner` would hand
    // them every record in the store — so before there were authorities it was
    // not a thing anybody could be.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("DEFINE USER ops AUTHORITIES operate PASSWORD 'correct horse battery';")
        .unwrap();

    let mut root = signed_in(&store, "root");
    assert_eq!(held(&mut root, "ops"), vec!["operate@store".to_owned()]);
}

#[test]
fn a_namespace_authority_reaches_that_namespace_and_not_its_neighbour() {
    // Rule 3, and the half of it that is easy to get wrong. `manage` over
    // `prod` is what creates and drops databases in `prod` — and the assertion
    // that matters is the *absence* of `staging`, because an authority that
    // quietly reached every namespace would satisfy a test that only looked at
    // the one it was granted on.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("DEFINE USER nadia ON NAMESPACE prod AUTHORITIES manage PASSWORD 'correct horse battery';")
        .unwrap();

    let mut root = signed_in(&store, "root");
    assert_eq!(held(&mut root, "nadia"), vec!["manage@prod".to_owned()]);
}

#[test]
fn reading_and_writing_a_namespace_confers_neither_creating_nor_dropping() {
    // Rule 4 — the one a ladder could not hold at any position, and therefore
    // the reason this model exists at all.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run(
            "DEFINE USER wilma ON NAMESPACE prod AUTHORITIES read, write \
             PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut root = signed_in(&store, "root");
    let holding = held(&mut root, "wilma");
    assert_eq!(
        holding,
        vec!["read@prod".to_owned(), "write@prod".to_owned()]
    );
    assert!(
        !holding.contains(&"manage@prod".to_owned()),
        "writing a namespace's records must not confer creating databases in it"
    );
}

#[test]
fn a_grant_adds_to_what_is_held_and_a_revocation_takes_only_what_it_names() {
    // The vocabulary's other half. A grant that *replaced* would make every
    // grant a silent revocation of every other one a user holds, which is the
    // failure that looks like nothing until the day somebody needs the second.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    root.run(
        "DEFINE USER kim ON NAMESPACE prod AUTHORITIES read PASSWORD 'correct horse battery';",
    )
    .unwrap();

    root.run("GRANT manage ON DATABASE prod.shop TO kim;")
        .unwrap();
    assert_eq!(
        held(&mut root, "kim"),
        vec!["manage@prod.shop".to_owned(), "read@prod".to_owned()],
        "a grant must add to what is held rather than replace it"
    );

    root.run("REVOKE read ON NAMESPACE prod FROM kim;").unwrap();
    assert_eq!(
        held(&mut root, "kim"),
        vec!["manage@prod.shop".to_owned()],
        "a revocation must take exactly what it names"
    );

    // Removing something nobody holds is not an error: the statement asks for a
    // user without it, and a user without it is what it leaves. A revocation
    // that failed halfway down a list would be worse than an idempotent one.
    root.run("REVOKE operate ON STORE FROM kim;").unwrap();
    assert_eq!(held(&mut root, "kim"), vec!["manage@prod.shop".to_owned()]);
}

#[test]
fn a_role_and_the_set_it_names_are_the_same_declaration() {
    // What keeps the number of role *names* at three while the number of
    // expressible sets is the whole lattice: `ROLE` is sugar, and it has to
    // produce exactly what spelling the set out produces or the two spellings
    // are two features.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    root.run(
        "DEFINE USER byrole ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER byset ON DATABASE prod.shop AUTHORITIES read, write, manage \
         PASSWORD 'correct horse battery';",
    )
    .unwrap();

    assert_eq!(held(&mut root, "byrole"), held(&mut root, "byset"));
}

#[test]
fn a_set_no_role_describes_is_reported_without_one() {
    // The honest half of keeping `role` in the record. It is written so that a
    // binary predating the authority set reads *something*, and it must never
    // read something wider than the truth — so a set no role fits carries no
    // role at all rather than the nearest one. `viewer` here would hand an older
    // binary a read this user does not hold.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    root.run(
        "DEFINE USER nadia ON NAMESPACE prod AUTHORITIES manage PASSWORD 'correct horse battery';",
    )
    .unwrap();

    let outcomes = root.run("INFO FOR USER nadia;").unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("expected a report");
    };
    assert!(
        !report.contains_key("role"),
        "a set no role describes must not be reported as a role: {report:?}"
    );

    // And the case that does fit still carries one, so the absence above is the
    // rule working rather than the field having been dropped.
    root.run("DEFINE USER seen ON prod.shop ROLE viewer PASSWORD 'correct horse battery';")
        .unwrap();
    let outcomes = root.run("INFO FOR USER seen;").unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("expected a report");
    };
    assert_eq!(report.get("role"), Some(&Value::from("viewer")));
}

#[test]
fn a_reach_is_named_by_keyword_so_a_table_can_never_be_read_as_one() {
    // The ambiguity that had to be made unrepresentable rather than resolved.
    // Before `STORE` was reserved, `GRANT read ON store TO ada` was a valid
    // statement meaning the *table* `store`; a reach spelled as a bare word
    // would have silently widened it to every namespace. Reserving the word
    // costs a table the name, and that cost is asserted here so it is a decision
    // on the record rather than a surprise in a release note.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    let refused = root
        .run("DEFINE TABLE store;")
        .expect_err("`store` is a reserved word");
    // The refusal names the keyword as the lexer spells it, which is the
    // evidence that the word was taken as a keyword rather than rejected for
    // some unrelated reason.
    assert!(refused.to_string().contains("STORE"), "{refused}");
}

// ---------------------------------------------------------------------------
// Enforcement
//
// Everything above asserts what can be *said*. Everything below asserts what a
// held set causes the store to *refuse*, which is the half that makes the model
// a permission system rather than a vocabulary.
//
// The three sets that exist only because something is disclosed — `BACKUP`,
// `CREATE`/`UPDATE`, and the consumer declaration in `consumers.rs` — each get a
// test holding the lesser authority alone. That is not thoroughness for its own
// sake: with a set rather than a rank, an arm carrying too *few* kinds is a
// silent privilege escalation, and `{write}` type-checks exactly like
// `{read, write}`. The compiler guards against a missing arm; only these guard
// against a thin one.
// ---------------------------------------------------------------------------

/// A session signed in and selected onto `prod.shop`.
fn working<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = signed_in(store, name);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// The refusal a statement produces, as text.
fn refused(session: &mut Session<'_>, statement: &str) -> String {
    match session.run(statement) {
        Ok(outcome) => panic!("{statement} was permitted: {outcome:?}"),
        Err(refusal) => refusal.to_string(),
    }
}

#[test]
fn writing_a_namespace_does_not_confer_creating_a_database_in_it() {
    // **Rule 4, enforced.** The one no ladder could hold, and the assertion the
    // whole goal exists for: `wilma` reads and writes everything in `prod` and
    // cannot create or drop a single container there.
    //
    // Both directions are asserted, because only the pair is the rule. A test
    // that showed the refusal alone would pass just as well against a store that
    // refused her everything.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run(
            "DEFINE USER wilma ON NAMESPACE prod AUTHORITIES read, write \
             PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut wilma = working(&store, "wilma");
    wilma
        .run("CREATE orders:1 = { total: 5 };")
        .expect("wilma writes records in the namespace she holds");
    wilma
        .run("SELECT * FROM orders;")
        .expect("and reads them back");

    for statement in [
        "DEFINE TABLE extra;",
        "DROP TABLE orders;",
        "DEFINE INDEX by_total ON orders FIELDS total;",
        "DEFINE FIELD total ON orders TYPE int;",
    ] {
        let refusal = refused(&mut wilma, statement);
        assert!(
            refusal.contains("manage"),
            "{statement} must name the authority she lacks: {refusal}"
        );
    }
}

#[test]
fn managing_a_namespace_does_not_confer_reading_a_record_in_it() {
    // The mirror, and it is the half that proves the independence runs both
    // ways. Under any ordering one of these two tests has to fail: put `manage`
    // above `write` and this user can read, put it below and the previous user
    // can manage.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run(
            "DEFINE USER nadia ON NAMESPACE prod AUTHORITIES manage \
             PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut nadia = working(&store, "nadia");
    nadia
        .run("DEFINE TABLE ledgers;")
        .expect("nadia manages the containers of the namespace she holds");

    let refusal = refused(&mut nadia, "SELECT * FROM orders;");
    assert!(refusal.contains("read"), "{refusal}");
    let refusal = refused(&mut nadia, "UPSERT orders:1 = { total: 5 };");
    assert!(refusal.contains("write"), "{refusal}");
}

#[test]
fn an_authority_over_one_database_does_not_answer_for_its_sibling() {
    // The case the coarse check cannot catch. `kim` holds `manage` *somewhere*,
    // so asking only "does she hold manage at all" answers yes for every
    // database in the store — and the containment that makes the answer right
    // for `shop` makes it wrong for `depot`. This is what the per-container pass
    // is for, and removing it leaves every other test in this file green.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run(
            "USE NAMESPACE prod; DEFINE DATABASE depot; \
             DEFINE USER kim ON NAMESPACE prod AUTHORITIES read \
             PASSWORD 'correct horse battery'; \
             GRANT manage ON DATABASE prod.shop TO kim;",
        )
        .unwrap();

    let mut kim = working(&store, "kim");
    kim.run("DEFINE TABLE invoices;")
        .expect("kim manages the database she was granted");

    kim.run("USE DATABASE depot;").unwrap();
    let refusal = refused(&mut kim, "DEFINE TABLE invoices;");
    assert!(
        refusal.contains("manage"),
        "a grant on one database must not answer for its sibling: {refusal}"
    );
}

#[test]
fn a_write_only_identity_can_write_and_can_learn_nothing() {
    // The headline case of the decomposition, and the reason `CREATE` and
    // `UPDATE` are classified `{read, write}` while `UPSERT` is not: those two
    // are *defined* by a claim about prior state, so their refusals answer a
    // question about it. Measured, not reasoned — `CREATE t:1` on an existing
    // record says *record 1 already exists*, and an oracle over record ids is
    // exactly what turns a write-only integration credential into an
    // enumeration tool.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run(
            "DEFINE USER ingest ON prod.shop AUTHORITIES write \
             PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut ingest = working(&store, "ingest");
    ingest
        .run("UPSERT orders:1 = { total: 5 };")
        .expect("the write that claims nothing about prior state");
    ingest
        .run("DELETE orders:9;")
        .expect("and the delete that answers ok for an absent record");

    for statement in [
        "CREATE orders:2 = { total: 5 };",
        "UPDATE orders:1 = { total: 6 };",
    ] {
        let refusal = refused(&mut ingest, statement);
        assert!(
            refusal.contains("read"),
            "{statement} discloses prior state and must demand the read: {refusal}"
        );
    }
    assert!(refused(&mut ingest, "SELECT * FROM orders;").contains("read"));
}

#[test]
fn running_the_cluster_does_not_confer_taking_a_copy_of_it() {
    // **The single most important row in the matrix.** Under the ladder this was
    // the store owner's class and that identity held `read` anyway, so the
    // requirement was invisible — a coincidence, not a rule. Decomposed, an
    // `operate`-only identity is meant to run the cluster and see no records,
    // and a backup file is every record there is.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("DEFINE USER ops AUTHORITIES operate PASSWORD 'correct horse battery';")
        .unwrap();

    let mut ops = signed_in(&store, "ops");
    ops.run("INFO FOR NODE;")
        .expect("ops runs the node, which is what operate is");

    let refusal = refused(&mut ops, "BACKUP;");
    assert!(
        refusal.contains("read"),
        "a backup is a complete disclosure and must demand the read: {refusal}"
    );
}

#[test]
fn governing_does_not_confer_reading_the_records_of_the_place_governed() {
    // Rule 9, enforced. An administrator declares users in a namespace without
    // holding a read over a single record in it — which is what separates
    // `govern` from the top of a ladder that had to contain everything below it.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run(
            "DEFINE USER gwen ON prod.shop AUTHORITIES govern \
             PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut gwen = working(&store, "gwen");
    gwen.run("DEFINE USER junior ON prod.shop ROLE viewer PASSWORD 'correct horse battery';")
        .expect("gwen governs the database she holds");

    let refusal = refused(&mut gwen, "SELECT * FROM orders;");
    assert!(refusal.contains("read"), "{refusal}");
}

#[test]
fn nobody_can_hand_out_an_authority_they_do_not_hold() {
    // **The one statement in the store that can escalate.** Every other is
    // bounded by what the caller may do now; a grant is bounded by what somebody
    // may do later, so a mistake here compounds — mint an identity above your
    // own and every other check becomes decorative, because the way past them
    // all is to be somebody else.
    //
    // `paula` governs `prod` and manages it. She may hand both out inside `prod`
    // and may hand out neither over the store, and the second half is the test:
    // containment runs downward and only downward.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run(
            "DEFINE USER paula ON NAMESPACE prod AUTHORITIES govern, manage \
             PASSWORD 'correct horse battery'; \
             DEFINE USER junior ON NAMESPACE prod AUTHORITIES read \
             PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut paula = signed_in(&store, "paula");
    paula
        .run("GRANT manage ON DATABASE prod.shop TO junior;")
        .expect("inside her own namespace, and she holds manage there");

    // Refused on `govern` and not on `manage`, and the order is the point: she
    // has no business handing out *anything* at store reach, so the broader gate
    // answers first and the kind is never consulted. The narrower failure is
    // asserted below, where the reach is one she does govern.
    let refusal = refused(&mut paula, "GRANT manage ON STORE TO junior;");
    assert!(
        refusal.contains("govern"),
        "a namespace authority must not mint a store-wide one: {refusal}"
    );

    // And the kind she was never given, at a reach she does govern. The reach
    // being right is exactly what makes this the interesting half: the refusal
    // has to come from the *kind*.
    let refusal = refused(&mut paula, "GRANT operate ON NAMESPACE prod TO junior;");
    assert!(
        refusal.contains("operate"),
        "you cannot give away what you were never given: {refusal}"
    );

    let mut root = signed_in(&store, "root");
    assert_eq!(
        held(&mut root, "junior"),
        vec!["manage@prod.shop".to_owned(), "read@prod".to_owned()],
        "only the grant that was permitted landed"
    );
}

#[test]
fn handing_out_authority_needs_governing_the_place_it_is_handed_out_in() {
    // The other question, and it is the same rule read from the other end.
    // `mo` holds every other kind over `prod` and does not govern it, so she may
    // do everything there herself and decide nothing about who else may.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run(
            "DEFINE USER mo ON NAMESPACE prod AUTHORITIES read, write, manage, operate \
             PASSWORD 'correct horse battery'; \
             DEFINE USER junior ON NAMESPACE prod AUTHORITIES read \
             PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut mo = signed_in(&store, "mo");
    let refusal = refused(&mut mo, "GRANT read ON NAMESPACE prod TO junior;");
    assert!(
        refusal.contains("govern"),
        "handing out authority is an act of governing, whatever else is held: {refusal}"
    );
}

#[test]
fn a_namespace_authority_can_now_hand_out_authority_inside_it() {
    // The case the M4 under-grant refused, paid off. `GRANT` demanded `govern`
    // at the **store** while nothing compared the caller's holdings to the reach
    // in the statement — safe, and too strict by exactly the rule the owner
    // asked for. The comparison exists now, so the class can come down.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run(
            "DEFINE USER nia ON NAMESPACE prod AUTHORITIES govern, read, write \
             PASSWORD 'correct horse battery'; \
             DEFINE USER junior ON NAMESPACE prod AUTHORITIES read \
             PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut nia = signed_in(&store, "nia");
    nia.run("GRANT write ON DATABASE prod.shop TO junior;")
        .expect("a namespace authority hands out authority inside their namespace");

    let mut root = signed_in(&store, "root");
    assert!(
        held(&mut root, "junior").contains(&"write@prod.shop".to_owned()),
        "the grant did not land"
    );
}
