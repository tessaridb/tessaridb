//! `INFO FOR` — what the catalog answers, and what it declines to.
//!
//! Two halves, and the second is the node's actual risk.
//!
//! The first is that every report is **read from the catalog**: it moves when
//! the catalog moves. A report assembled once and kept beside the schema would
//! pass a single-shot assertion and drift silently, so every test here reads,
//! changes the catalog, and reads again.
//!
//! The second is that a report says only what the caller could have found out
//! anyway. Three of the five subjects name no table, so the grant check "every
//! table this statement names is granted" passes over them **vacuously** — the
//! shape that let a grant-governed owner take a whole backup, and the shape a
//! multi-hop traversal took through an ungranted table. Here the answer is a
//! narrowing rather than a refusal, and these tests are what hold it shut.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

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

/// An open store with a tenancy and two tables, one of them described.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE staff SCHEMAFULL;\n\
             DEFINE COLLECTION orders;\n\
             DEFINE FIELD name ON staff TYPE string;\n\
             DEFINE FIELD salary ON staff TYPE int;\n\
             DEFINE INDEX by_name ON staff FIELDS name;\n\
             DEFINE INDEX by_salary ON staff FIELDS salary;",
        )
        .unwrap();
    session
}

/// The report a script's last statement answered with.
fn report(session: &mut Session<'_>, script: &str) -> Value {
    let outcomes = session.run(script).unwrap();
    match outcomes.last() {
        Some(Outcome::Value(value)) => value.clone(),
        other => panic!("expected a report, got {other:?}"),
    }
}

/// One named part of a report, as a list of strings.
fn listed(report: &Value, field: &str) -> Vec<String> {
    let Value::Object(fields) = report else {
        panic!("expected an object, got {report:?}");
    };
    let Some(Value::Array(items)) = fields.get(field) else {
        panic!("expected {field} to be a list in {report:?}");
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(text) => text.clone(),
            Value::Object(named) => match named.get("name") {
                Some(Value::String(text)) => text.clone(),
                _ => panic!("expected a name in {item:?}"),
            },
            other => panic!("expected a name, got {other:?}"),
        })
        .collect()
}

#[test]
fn the_store_lists_its_namespaces_and_never_the_one_the_catalog_lives_in() {
    // The knowledge base predicted this statement as the change that would give
    // the system tenancy a name so it could be listed. It has none to give: it
    // was never created through the language, so it has no definition record and
    // this scan cannot produce one. Absent rather than filtered.
    let store = store();
    let mut session = ready(&store);
    let listing = listed(&report(&mut session, "INFO FOR STORE;"), "namespaces");
    assert_eq!(listing, vec!["prod".to_owned()]);

    session.run("DEFINE NAMESPACE staging;").unwrap();
    let after = listed(&report(&mut session, "INFO FOR STORE;"), "namespaces");
    assert_eq!(after, vec!["prod".to_owned(), "staging".to_owned()]);
}

#[test]
fn a_namespace_lists_its_own_databases_and_not_another_namespaces() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE NAMESPACE other; USE NAMESPACE other; DEFINE DATABASE elsewhere;")
        .unwrap();
    session.run("USE NAMESPACE prod;").unwrap();

    let listing = listed(&report(&mut session, "INFO FOR NAMESPACE;"), "databases");
    assert_eq!(listing, vec!["shop".to_owned()]);

    session.run("DEFINE DATABASE warehouse;").unwrap();
    let after = listed(&report(&mut session, "INFO FOR NAMESPACE;"), "databases");
    assert_eq!(after, vec!["shop".to_owned(), "warehouse".to_owned()]);
}

#[test]
fn a_database_lists_its_tables_and_a_new_one_appears() {
    let store = store();
    let mut session = ready(&store);
    let listing = listed(&report(&mut session, "INFO FOR DATABASE;"), "tables");
    assert_eq!(listing, vec!["orders".to_owned(), "staff".to_owned()]);

    session.run("DEFINE COLLECTION audit;").unwrap();
    let after = listed(&report(&mut session, "INFO FOR DATABASE;"), "tables");
    assert!(after.contains(&"audit".to_owned()), "{after:?}");
}

#[test]
fn a_buckets_chunk_table_is_not_listed_because_nothing_can_name_it() {
    // A bucket's bytes live in a companion table whose name carries a byte no
    // identifier can hold, which is what makes `SELECT * FROM media` answer with
    // files rather than chunks. A listing is the one place that enumerates
    // instead of resolving, so it is the one place that could undo it.
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE BUCKET media;").unwrap();
    let listing = listed(&report(&mut session, "INFO FOR DATABASE;"), "tables");
    assert!(listing.contains(&"media".to_owned()), "{listing:?}");
    assert!(
        listing.iter().all(|name| !name.contains('\u{1}')),
        "the chunk table was listed: {listing:?}"
    );
}

#[test]
fn a_table_reports_its_shape_its_fields_and_its_indexes() {
    let store = store();
    let mut session = ready(&store);
    let described = report(&mut session, "INFO FOR TABLE staff;");
    let Value::Object(fields) = &described else {
        panic!("expected an object");
    };
    assert_eq!(fields.get("table"), Some(&Value::from("staff")));
    assert_eq!(fields.get("schemafull"), Some(&Value::Bool(true)));
    assert_eq!(fields.get("edge"), Some(&Value::Bool(false)));
    assert_eq!(fields.get("bucket"), Some(&Value::Bool(false)));
    assert_eq!(
        listed(&described, "fields"),
        vec!["name".to_owned(), "salary".to_owned()]
    );
    assert_eq!(
        listed(&described, "indexes"),
        vec!["by_name".to_owned(), "by_salary".to_owned()]
    );

    // And it moves when the catalog does, which is the difference between
    // reading the catalog and rendering something kept beside it.
    session.run("DROP INDEX by_salary ON staff;").unwrap();
    let after = report(&mut session, "INFO FOR TABLE staff;");
    assert_eq!(listed(&after, "indexes"), vec!["by_name".to_owned()]);
}

#[test]
fn a_field_reports_what_was_declared_about_it() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE FIELD tier ON staff TYPE string ASSERT $value != 'x';")
        .unwrap();
    let described = report(&mut session, "INFO FOR TABLE staff;");
    let Value::Object(fields) = &described else {
        panic!("expected an object");
    };
    let Some(Value::Array(declared)) = fields.get("fields") else {
        panic!("expected a field list");
    };
    let tier = declared
        .iter()
        .find(|field| match field {
            Value::Object(named) => named.get("name") == Some(&Value::from("tier")),
            _ => false,
        })
        .expect("tier was declared");
    let Value::Object(named) = tier else {
        panic!("expected an object");
    };
    assert_eq!(named.get("type"), Some(&Value::from("string")));
    assert_eq!(named.get("required"), Some(&Value::Bool(false)));
    // The stored constraint rather than the sentence that described it.
    assert!(named.contains_key("assert"), "{named:?}");
}

// The reports above are what an unrestricted caller sees. Everything below is
// about a caller who is restricted, which is where the node's risk lives.

/// A store with an owner and a grant-governed editor.
fn governed(store: &Store) -> Session<'_> {
    let mut session = ready(store);
    session
        .run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';")
        .unwrap();
    session
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

#[test]
fn a_database_report_lists_only_the_tables_the_caller_was_granted() {
    // `INFO FOR DATABASE` names no table, so the grant loop passes over it
    // vacuously — the `BACKUP` shape. The narrowing is what stands in for a
    // refusal, and this is the test that says so.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("GRANT read ON orders TO ada;")
        .unwrap();

    let mut ada = signed_in(&store, "ada");
    let listing = listed(&report(&mut ada, "INFO FOR DATABASE;"), "tables");
    assert_eq!(listing, vec!["orders".to_owned()]);
}

#[test]
fn a_table_the_caller_was_not_granted_is_refused_and_not_merely_omitted() {
    // The other half of the same rule: a listing narrows, but naming a table
    // asks about it, and asking about one nobody granted is the question a
    // `SELECT` from it would be.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("GRANT read ON orders TO ada;")
        .unwrap();

    let mut ada = signed_in(&store, "ada");
    let refused = ada
        .run("INFO FOR TABLE staff;")
        .expect_err("staff was never granted");
    let said = refused.to_string();
    assert!(said.contains("ada"), "{said}");
    assert!(said.contains("staff"), "{said}");
}

#[test]
fn a_field_grant_hides_the_declaration_and_the_index_that_names_it() {
    // A field permission edits rather than refuses: `salary` is removed from the
    // record before anything looks at it, so every read this caller makes
    // already hides it. A report naming it as a declared field would disclose
    // exactly what those reads hide — and so would an index called `by_salary`,
    // because an index is named after the values it projects.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("GRANT read ON staff FIELDS name TO ada;")
        .unwrap();

    let mut ada = signed_in(&store, "ada");
    let described = report(&mut ada, "INFO FOR TABLE staff;");
    assert_eq!(listed(&described, "fields"), vec!["name".to_owned()]);
    assert_eq!(listed(&described, "indexes"), vec!["by_name".to_owned()]);
}

#[test]
fn a_scoped_user_sees_one_namespace_and_one_database() {
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    root.run("DEFINE NAMESPACE other;").unwrap();
    root.run("DEFINE DATABASE warehouse;").unwrap();

    let mut ada = signed_in(&store, "ada");
    assert_eq!(
        listed(&report(&mut ada, "INFO FOR STORE;"), "namespaces"),
        vec!["prod".to_owned()]
    );
    assert_eq!(
        listed(&report(&mut ada, "INFO FOR NAMESPACE;"), "databases"),
        vec!["shop".to_owned()]
    );

    // The owner is not scoped, and sees both.
    let mut root = signed_in(&store, "root");
    let seen = listed(&report(&mut root, "INFO FOR STORE;"), "namespaces");
    assert_eq!(seen, vec!["other".to_owned(), "prod".to_owned()]);
}

#[test]
fn asking_about_a_user_needs_an_owner() {
    // The one subject that refuses instead of narrowing. There is no smaller
    // truthful answer about who may do what, and a partial one reads as the
    // whole answer.
    let store = store();
    governed(&store);
    let mut ada = signed_in(&store, "ada");
    let refused = ada
        .run("INFO FOR USER ada;")
        .expect_err("an editor may not read the permission system");
    assert!(refused.to_string().contains("govern"), "{refused}");
}

#[test]
fn listing_users_needs_an_owner_and_refuses_rather_than_narrowing() {
    // The same rule as the singular subject, and it is the whole reason a
    // listing was safe to add: a list narrowed to what an `editor` may see would
    // be a partial account of who may do what, and a partial account reads as
    // the whole one.
    let store = store();
    governed(&store);
    let mut ada = signed_in(&store, "ada");
    let refused = ada
        .run("INFO FOR USERS;")
        .expect_err("an editor may not read the permission system");
    assert!(refused.to_string().contains("govern"), "{refused}");
}

#[test]
fn a_database_owner_is_listed_their_own_tenancy_and_not_the_store_owner() {
    // The property the containment rule exists for, and the one that would leak
    // quietly if it were equality or nothing: passing the permission check says
    // somebody administers *something*, and it does not say they administer the
    // whole store. A tenancy owner must not learn who owns the store.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("DEFINE USER dbowner ON prod.shop ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();

    let mut owner = signed_in(&store, "dbowner");
    let named = |held: &Value| -> Vec<String> {
        let Value::Object(fields) = held else {
            panic!("expected an object");
        };
        let Some(Value::Array(users)) = fields.get("users") else {
            panic!("expected a user list");
        };
        users
            .iter()
            .map(|user| {
                let Value::Object(one) = user else {
                    panic!("expected an object per user");
                };
                match one.get("user") {
                    Some(Value::String(name)) => name.clone(),
                    other => panic!("expected a name, found {other:?}"),
                }
            })
            .collect()
    };

    let mut seen = named(&report(&mut owner, "INFO FOR USERS;"));
    seen.sort();
    assert_eq!(seen, vec!["ada".to_owned(), "dbowner".to_owned()]);

    // And the store owner sees all three, so the absence above is the rule
    // working rather than the listing being empty for some other reason.
    let mut root = signed_in(&store, "root");
    let mut everyone = named(&report(&mut root, "INFO FOR USERS;"));
    everyone.sort();
    assert_eq!(
        everyone,
        vec!["ada".to_owned(), "dbowner".to_owned(), "root".to_owned()]
    );
}

#[test]
fn a_listing_carries_no_password_hash_and_no_grants() {
    // Two absences for two reasons. The hash for `a_user_report_never_carries_
    // the_password_hash`'s reason. The grants because they are per-user detail
    // and belong to the subject that examines one user rather than counts them —
    // an assertion here is what stops that decision being undone by accident.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("GRANT read ON orders FIELDS total TO ada;")
        .unwrap();

    let mut root = signed_in(&store, "root");
    let listing = format!("{:?}", report(&mut root, "INFO FOR USERS;"));
    assert!(!listing.contains("argon2"), "{listing}");
    assert!(!listing.contains("$"), "{listing}");
    assert!(!listing.contains("grants"), "{listing}");
}

#[test]
fn an_owner_reads_a_users_role_tenancy_and_grants() {
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("GRANT read ON orders FIELDS total TO ada;")
        .unwrap();

    let mut root = signed_in(&store, "root");
    let described = report(&mut root, "INFO FOR USER ada;");
    let Value::Object(fields) = &described else {
        panic!("expected an object");
    };
    assert_eq!(fields.get("user"), Some(&Value::from("ada")));
    assert_eq!(fields.get("role"), Some(&Value::from("editor")));
    assert_eq!(fields.get("namespace"), Some(&Value::from("prod")));
    assert_eq!(fields.get("database"), Some(&Value::from("shop")));

    let Some(Value::Array(grants)) = fields.get("grants") else {
        panic!("expected a grant list");
    };
    assert_eq!(grants.len(), 1);
    let Value::Object(grant) = &grants[0] else {
        panic!("expected an object");
    };
    assert_eq!(grant.get("table"), Some(&Value::from("orders")));
    // `FIELDS` names which fields may be **read**, so the language refuses it
    // beside a `write` — the report shows the grant that was actually stored.
    assert_eq!(
        grant.get("verbs"),
        Some(&Value::Array(vec![Value::from("read")]))
    );
    assert_eq!(
        grant.get("fields"),
        Some(&Value::Array(vec![Value::from("total")]))
    );
}

#[test]
fn a_user_report_never_carries_the_password_hash() {
    // The stored definition holds it, because that is what the catalog keeps.
    // The report is built field by field rather than from that value, so the
    // hash does not travel to whoever asked or to whatever logs the answer.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    let described = report(&mut root, "INFO FOR USER ada;");
    let Value::Object(fields) = &described else {
        panic!("expected an object");
    };
    assert!(!fields.contains_key("secret"), "{fields:?}");
    assert!(!fields.contains_key("password"), "{fields:?}");
    let printed = format!("{described:?}");
    assert!(!printed.contains("argon2"), "{printed}");
    assert!(!printed.contains(PASSWORD), "{printed}");
}

#[test]
fn a_grant_outliving_the_table_it_names_reports_the_absence() {
    // Dropping a table removes its definition and releases its name; the grants
    // on it stay. The report says so with an absence rather than raising, and
    // this test exists because that was a claim in a doc comment before it was
    // a claim a build could check.
    let store = store();
    governed(&store);
    let mut root = signed_in(&store, "root");
    root.run("GRANT read ON orders TO ada;").unwrap();
    root.run("DROP TABLE orders;").unwrap();

    let described = report(&mut root, "INFO FOR USER ada;");
    let Value::Object(fields) = &described else {
        panic!("expected an object");
    };
    let Some(Value::Array(grants)) = fields.get("grants") else {
        panic!("expected a grant list");
    };
    assert_eq!(grants.len(), 1);
    let Value::Object(grant) = &grants[0] else {
        panic!("expected an object");
    };
    assert_eq!(grant.get("table"), Some(&Value::None));
}

// The declaration a report carries, and the round trip that is the only proof
// it is a declaration rather than a sentence that resembles one.
//
// A description can be checked by reading it. A declaration cannot: it is a
// claim about what running it would produce, and the only way to check a claim
// about running something is to run it. So every test below carries the script
// to a **second, empty store** and compares the report it gets there with the
// report it came from. Comparing whole reports rather than chosen fields is
// deliberate — a field nobody thought to assert is exactly the field a
// declaration silently drops.

/// One named part of a report, as text.
fn text(report: &Value, field: &str) -> Option<String> {
    let Value::Object(fields) = report else {
        panic!("expected an object, got {report:?}");
    };
    match fields.get(field) {
        Some(Value::String(held)) => Some(held.clone()),
        None => None,
        other => panic!("expected {field} to be text in {other:?}"),
    }
}

/// An empty store with the same tenancy selected, and nothing else.
fn elsewhere(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;")
        .unwrap();
    session
}

#[test]
fn a_definition_re_creates_the_table_it_describes() {
    let store = store();
    let mut session = ready(&store);
    // Every part of a field declaration at once, because each is rendered by a
    // different branch and a script that carries four of five is the failure
    // this test exists to catch.
    session
        .run(
            "DEFINE FIELD tier ON staff TYPE 'draft' | 'live' REQUIRED DEFAULT 'draft';\n\
             DEFINE FIELD level ON staff TYPE int ASSERT ($value > 0 AND $value < 150);\n\
             DEFINE FIELD note ON staff TYPE string ASSERT $value != 'it\\'s';\n\
             DEFINE INDEX by_tier ON staff FIELDS tier UNIQUE;",
        )
        .unwrap();

    let described = report(&mut session, "INFO FOR TABLE staff;");
    let script = text(&described, "definition").expect("a definition");

    // `store` is the local binding by now, so the helper is named through the
    // module rather than shadowed out of reach.
    let second = self::store();
    let mut fresh = elsewhere(&second);
    fresh
        .run(&script)
        .unwrap_or_else(|error| panic!("the definition did not run: {error}\n{script}"));

    let again = report(&mut fresh, "INFO FOR TABLE staff;");
    assert_eq!(described, again, "\nfrom:\n{script}");
}

#[test]
fn a_collection_is_declared_back_as_a_collection_and_not_as_a_lenient_table() {
    // The two accept the same writes, so a round trip that only compared
    // behaviour would pass while the word was lost. The report carries the flag
    // and the script carries the word, and this asserts both — the flag because
    // it is what makes the comparison able to fail, and the word because the
    // flag could be reported and still not reach the text.
    let store = store();
    let mut session = ready(&store);
    let described = report(&mut session, "INFO FOR TABLE orders;");
    let Value::Object(fields) = &described else {
        panic!("expected an object");
    };
    assert_eq!(fields.get("collection"), Some(&Value::Bool(true)));

    let script = text(&described, "definition").expect("a definition");
    assert!(
        script.contains("DEFINE COLLECTION orders"),
        "a collection was declared back as something else:\n{script}"
    );

    // `store` is the local binding by now, so the helper is named through the
    // module rather than shadowed out of reach.
    let second = self::store();
    let mut fresh = elsewhere(&second);
    fresh.run(&script).unwrap();
    assert_eq!(described, report(&mut fresh, "INFO FOR TABLE orders;"));
}

#[test]
fn a_spatial_index_is_reported_and_declared_as_spatial() {
    // A spatial index writes the cells covering a shape; an ordinary one writes
    // the value. The report carried three of the four kinds and a spatial index
    // read back as ordinary — a report saying the index answers ranges when it
    // answers candidates.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE FIELD ground ON staff TYPE geometry;\n\
             DEFINE INDEX by_ground ON staff FIELDS ground SPATIAL;",
        )
        .unwrap();

    let described = report(&mut session, "INFO FOR TABLE staff;");
    let script = text(&described, "definition").expect("a definition");
    assert!(script.contains("SPATIAL"), "{script}");

    // `store` is the local binding by now, so the helper is named through the
    // module rather than shadowed out of reach.
    let second = self::store();
    let mut fresh = elsewhere(&second);
    fresh.run(&script).unwrap();
    assert_eq!(described, report(&mut fresh, "INFO FOR TABLE staff;"));
}

#[test]
fn a_caller_who_may_not_see_every_field_gets_no_definition_at_all() {
    // The report is narrowed for this caller and that is a truthful
    // *description*. A **declaration** built from the same subset is not: it
    // claims to re-create the table and would re-create a different one, and it
    // would disclose through the definition precisely what the field grant
    // removes from every read they make.
    let store = store();
    governed(&store);
    signed_in(&store, "root")
        .run("GRANT read ON staff FIELDS name TO ada;")
        .unwrap();

    let mut ada = signed_in(&store, "ada");
    let described = report(&mut ada, "INFO FOR TABLE staff;");
    assert_eq!(text(&described, "definition"), None);
    let said = text(&described, "undefinable").expect("a reason");
    assert!(said.contains("hidden"), "{said}");
    assert!(
        !said.contains("salary"),
        "the reason named the hidden field"
    );
}

#[test]
fn a_constraint_with_no_faithful_spelling_withholds_the_definition_and_names_the_field() {
    // A duration is written `1h` and displayed `90.000000000s`, which lexes as a
    // float and a stray name. Writing the display would produce a script that
    // fails to parse — or, on another value, one that parses as something else.
    // Withholding is the answer, and naming the field is what makes it fixable
    // rather than merely safe.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE FIELD wait ON staff TYPE duration ASSERT $value > 1h;")
        .unwrap();

    let described = report(&mut session, "INFO FOR TABLE staff;");
    assert_eq!(text(&described, "definition"), None);
    let said = text(&described, "undefinable").expect("a reason");
    assert!(said.contains("wait"), "{said}");
}

// Listing the tenancies a caller may reach, and the one property that keeps the
// listing safe.
//
// Both statements read every definition and drop the ones the caller may not
// reach. That is a filter applied to the scan rather than pushed into it, and
// the reason it is safe here is narrow and worth writing down: the report
// carries **names and nothing else**. There is no total, no page, no aggregate
// and no cursor, so there is no channel through which a dropped tenancy could
// still say it exists. Add a count and that stops being true — which is what
// the last test below is for.

/// A store with an owner, a namespace-scoped editor and a database-scoped one.
///
/// Two scopes rather than one, because the difference between them is exactly
/// what `INFO FOR NAMESPACE` answers differently and a single scoped user cannot
/// show it.
fn two_scopes(store: &Store) -> Session<'_> {
    let session = governed(store);
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("DEFINE USER nina ON NAMESPACE prod ROLE editor PASSWORD 'correct horse battery';")
        .unwrap();
    root.run("DEFINE NAMESPACE other;").unwrap();
    root.run("USE NAMESPACE prod; DEFINE DATABASE payroll;")
        .unwrap();
    session
}

#[test]
fn a_namespace_scoped_user_lists_every_database_in_it_and_no_namespace_beside_it() {
    // The assertion is **nina's** list, not the difference between hers and
    // somebody else's: a diff passes just as well when the filter drops the same
    // row from both, and that is the failure it is supposed to catch.
    let store = store();
    two_scopes(&store);

    let mut nina = signed_in(&store, "nina");
    assert_eq!(
        listed(&report(&mut nina, "INFO FOR STORE;"), "namespaces"),
        vec!["prod".to_owned()],
        "a namespace-scoped user was shown a namespace they cannot select"
    );
    // Every database in her own namespace, including the one she holds no grant
    // in. That is not a leak: her tenancy lets her `USE DATABASE payroll`, which
    // succeeds and tells her it exists, so the listing says nothing she could
    // not already have found out. What it does not do is show her its tables —
    // `INFO FOR DATABASE` narrows those to what she was granted.
    assert_eq!(
        listed(&report(&mut nina, "INFO FOR NAMESPACE;"), "databases"),
        vec!["payroll".to_owned(), "shop".to_owned()]
    );
}

#[test]
fn a_database_scoped_user_lists_only_their_own_database() {
    // The second user of the pair, asserted on her own answer. `ada` is declared
    // `ON prod.shop` where `nina` is declared `ON NAMESPACE prod`, so the two
    // ask the same statement of the same store and are owed different answers.
    let store = store();
    two_scopes(&store);

    let mut ada = signed_in(&store, "ada");
    assert_eq!(
        listed(&report(&mut ada, "INFO FOR STORE;"), "namespaces"),
        vec!["prod".to_owned()]
    );
    assert_eq!(
        listed(&report(&mut ada, "INFO FOR NAMESPACE;"), "databases"),
        vec!["shop".to_owned()],
        "a database-scoped user was shown a database beside their own"
    );
}

#[test]
fn a_listing_carries_the_names_and_nothing_that_counts_what_was_dropped() {
    // The ratchet under the two tests above. Both statements filter a scan
    // rather than narrowing the read, and that is safe only while the report has
    // no second field — a total, a page or an "of N" would report the tenancies
    // the filter removed, in the one number nobody would think to redact.
    //
    // This fails the day a field is added, which is the point: adding one is a
    // decision, and this is where it gets made rather than noticed later.
    let store = store();
    two_scopes(&store);
    let mut nina = signed_in(&store, "nina");

    for (statement, expected) in [
        ("INFO FOR STORE;", "namespaces"),
        ("INFO FOR NAMESPACE;", "databases"),
    ] {
        let Value::Object(fields) = report(&mut nina, statement) else {
            panic!("expected an object from {statement}");
        };
        let keys: Vec<&str> = fields.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![expected],
            "{statement} grew a field beside the names"
        );
    }
}

// Every part the report shows, and the statement that changes it.
//
// A report is worth less than it looks if reading it is the end of the road: the
// operator who can see that a field is `REQUIRED` and cannot make it optional
// has been given a diagnosis and no treatment. So each part below is altered and
// then **re-read**, because a statement that returns success is not evidence
// that the catalog moved — that is the difference this file exists to hold.
//
// Three parts have no `ALTER`, and they are asserted as unchanging rather than
// left to be discovered: the table's **name**, and the **word it was declared
// with** (`edge`, `bucket`, `collection`). The last test says so out loud.

/// One field of one field's declaration, out of a table report.
fn declared(report: &Value, field: &str, part: &str) -> Option<Value> {
    let Value::Object(fields) = report else {
        panic!("expected an object, got {report:?}");
    };
    let Some(Value::Array(declared)) = fields.get("fields") else {
        panic!("expected a field list in {report:?}");
    };
    declared.iter().find_map(|held| match held {
        Value::Object(named) if named.get("name") == Some(&Value::from(field)) => {
            named.get(part).cloned()
        }
        _ => None,
    })
}

#[test]
fn every_part_of_a_field_that_is_shown_can_be_altered_and_the_report_follows() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE ANALYZER simple FILTERS lowercase;")
        .unwrap();
    // An empty table, because altering a declaration re-checks the rows already
    // stored and this test is about the catalog rather than about that check.
    session.run("DEFINE TABLE shapes (label string);").unwrap();

    let described = report(&mut session, "INFO FOR TABLE shapes;");
    assert_eq!(
        declared(&described, "label", "type"),
        Some(Value::from("string"))
    );
    assert_eq!(
        declared(&described, "label", "required"),
        Some(Value::Bool(false))
    );

    // Type, required, default, analyzer and assert, all five in the one
    // statement that replaces a declaration.
    session
        .run(
            "ALTER TABLE shapes ALTER FIELD label TYPE 'round' | 'square' REQUIRED \
             DEFAULT 'round' ANALYZER simple ASSERT $value != 'oval';",
        )
        .unwrap();
    let after = report(&mut session, "INFO FOR TABLE shapes;");
    assert_eq!(
        declared(&after, "label", "type"),
        Some(Value::from("'round' | 'square'"))
    );
    assert_eq!(
        declared(&after, "label", "required"),
        Some(Value::Bool(true))
    );
    assert_eq!(
        declared(&after, "label", "default"),
        Some(Value::from("'round'"))
    );
    assert_eq!(
        declared(&after, "label", "analyzer"),
        Some(Value::from("simple"))
    );
    assert!(declared(&after, "label", "assert").is_some());

    // A field can be added and taken away, and the report follows both ways.
    session
        .run("ALTER TABLE shapes ADD FIELD note TYPE string;")
        .unwrap();
    assert!(
        listed(&report(&mut session, "INFO FOR TABLE shapes;"), "fields")
            .contains(&"note".to_owned())
    );
    session.run("ALTER TABLE shapes DROP FIELD note;").unwrap();
    assert!(
        !listed(&report(&mut session, "INFO FOR TABLE shapes;"), "fields")
            .contains(&"note".to_owned())
    );
}

#[test]
fn strictness_is_altered_in_both_directions_and_the_report_follows_each_way() {
    let store = store();
    let mut session = ready(&store);

    for (statement, expected) in [
        ("ALTER TABLE staff SET SCHEMALESS;", false),
        ("ALTER TABLE staff SET SCHEMAFULL;", true),
    ] {
        session.run(statement).unwrap();
        let Value::Object(fields) = report(&mut session, "INFO FOR TABLE staff;") else {
            panic!("expected an object");
        };
        assert_eq!(
            fields.get("schemafull"),
            Some(&Value::Bool(expected)),
            "{statement} did not reach the report"
        );
    }
}

#[test]
fn an_index_is_changed_by_replacing_it_and_the_table_is_never_dropped() {
    // There is no `ALTER INDEX`, and an index has nothing an alteration could
    // change in place: its projected fields and its kind are what its entries
    // are keyed by, so changing either rewrites every entry. Dropping and
    // re-declaring says that plainly, and the records are untouched throughout —
    // which is the property this asserts, since it is the one a caller cares
    // about and the one a rebuild would quietly break.
    let store = store();
    let mut session = ready(&store);
    session
        .run("INSERT INTO staff (name, salary) VALUES ('ada', 1), ('grace', 2);")
        .unwrap();

    session.run("DROP INDEX by_name ON staff;").unwrap();
    session
        .run("DEFINE INDEX by_name ON staff FIELDS name UNIQUE;")
        .unwrap();

    let after = report(&mut session, "INFO FOR TABLE staff;");
    let Value::Object(fields) = &after else {
        panic!("expected an object");
    };
    let Some(Value::Array(indexes)) = fields.get("indexes") else {
        panic!("expected an index list");
    };
    let by_name = indexes
        .iter()
        .find_map(|held| match held {
            Value::Object(named) if named.get("name") == Some(&Value::from("by_name")) => {
                Some(named.clone())
            }
            _ => None,
        })
        .expect("by_name was re-declared");
    assert_eq!(by_name.get("unique"), Some(&Value::Bool(true)));

    // The rows are still there, and the new index answers over them.
    let rows = session
        .run("SELECT * FROM staff WHERE name = 'ada';")
        .unwrap();
    assert!(
        !rows.is_empty(),
        "the records did not survive the replacement"
    );
}

#[test]
fn the_word_a_table_was_declared_with_survives_every_alteration_there_is() {
    // The three parts with no `ALTER`, asserted as unchanging rather than left
    // for somebody to discover. They are not an oversight: `edge`, `bucket` and
    // `collection` say what a record *is* rather than what may be written to it,
    // so changing one would reinterpret every row already stored — and a table's
    // name is what its grants, its indexes and every reference to it are keyed
    // by. Both are changes made by declaring the thing you meant and moving the
    // records, which the language already says.
    let store = store();
    let mut session = ready(&store);
    let before = report(&mut session, "INFO FOR TABLE orders;");

    session.run("ALTER TABLE orders SET SCHEMAFULL;").unwrap();
    session.run("ALTER TABLE orders SET SCHEMALESS;").unwrap();
    session
        .run("ALTER TABLE orders ADD FIELD code TYPE string;")
        .unwrap();
    session.run("ALTER TABLE orders DROP FIELD code;").unwrap();

    assert_eq!(
        before,
        report(&mut session, "INFO FOR TABLE orders;"),
        "an alteration changed the word the table was declared with"
    );
}

#[test]
fn a_table_reports_how_it_names_a_record_the_caller_did_not_name() {
    // The property this asserts is not that the field is present but that it is
    // *read from the catalog*: `staff` and `orders` were declared without the
    // word, and a report that hard-coded a default would be indistinguishable
    // from one that read it — right up until a table declared otherwise.
    let store = store();
    let mut session = ready(&store);
    for table in ["staff", "orders"] {
        let described = report(&mut session, &format!("INFO FOR TABLE {table};"));
        let Value::Object(fields) = &described else {
            panic!("expected an object");
        };
        assert_eq!(
            fields.get("identity"),
            Some(&Value::from("int")),
            "{table} was reported as naming records some other way"
        );
    }

    session
        .run("DEFINE TABLE sessions IDENTITY uuid SCHEMALESS;")
        .unwrap();
    let described = report(&mut session, "INFO FOR TABLE sessions;");
    let Value::Object(fields) = &described else {
        panic!("expected an object");
    };
    assert_eq!(fields.get("identity"), Some(&Value::from("uuid")));
}

#[test]
fn the_naming_scheme_survives_the_round_trip_through_the_definition() {
    // The failure this refuses is the one the `collection` test above describes,
    // in its worst form. A report can carry the flag and still not reach the
    // text, and a declaration taken from `INFO FOR TABLE` and replayed onto
    // another store would then build a table that names every record it is ever
    // given by a different scheme — silently, because both schemes work.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE sessions IDENTITY uuid SCHEMALESS;\n\
             DEFINE COLLECTION invitations IDENTITY uuid;",
        )
        .unwrap();

    for (table, expected) in [
        ("sessions", "DEFINE TABLE sessions"),
        ("invitations", "DEFINE COLLECTION invitations"),
    ] {
        let described = report(&mut session, &format!("INFO FOR TABLE {table};"));
        let script = text(&described, "definition").expect("a definition");
        assert!(script.contains(expected), "{script}");
        assert!(
            script.contains("IDENTITY uuid"),
            "the naming scheme did not reach the declaration:\n{script}"
        );
    }

    // And the declaration a table without the word produces still says what it
    // means, rather than leaning on whatever the reading build's default is.
    let described = report(&mut session, "INFO FOR TABLE staff;");
    let script = text(&described, "definition").expect("a definition");
    assert!(script.contains("IDENTITY int"), "{script}");
}

#[test]
fn a_definition_carrying_the_naming_scheme_can_be_replayed() {
    // The round trip closed at both ends: the script `INFO FOR TABLE` hands out
    // is fed back to the parser and the table it builds is asked the same
    // question. A rendering that emitted a word the grammar does not accept
    // would pass every assertion above and fail here.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE sessions IDENTITY uuid SCHEMALESS;")
        .unwrap();
    let script = text(
        &report(&mut session, "INFO FOR TABLE sessions;"),
        "definition",
    )
    .expect("a definition");

    // `store` is the local binding by now, so the helper is named through the
    // module rather than shadowed out of reach.
    let elsewhere = self::store();
    let mut replayed = Session::new(&elsewhere);
    replayed
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;",
        )
        .unwrap();
    replayed.run(&script).unwrap();

    let described = report(&mut replayed, "INFO FOR TABLE sessions;");
    let Value::Object(fields) = &described else {
        panic!("expected an object");
    };
    assert_eq!(
        fields.get("identity"),
        Some(&Value::from("uuid")),
        "the replayed table names records some other way"
    );
}

/// `INFO FOR BUCKET` reports the name and the ceiling the bucket declared.
///
/// The bucket was the one engine with no `INFO` subject. That absence was not
/// cosmetic: it is why the HTTP listing route had no statement to ask whether a
/// name was a bucket, and answered `200` with an empty listing for a plain
/// table — a confident wrong answer, which is the class this store refuses.
#[test]
fn a_bucket_reports_its_name_and_the_largest_file_it_takes() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE BUCKET avatars MAX 5242880;").unwrap();

    let described = report(&mut session, "INFO FOR BUCKET avatars;");
    let Value::Object(fields) = &described else {
        panic!("expected an object, got {described:?}");
    };
    assert_eq!(fields.get("name"), Some(&Value::from("avatars")));
    assert_eq!(fields.get("max"), Some(&Value::from(5_242_880_i64)));
}

/// An unbounded bucket answers `NONE` rather than a zero.
///
/// A zero would be the ceiling that admits no file at all — which is a
/// declaration this store refuses outright — so reporting absence as zero would
/// describe every ordinary bucket as one nobody can write to.
#[test]
fn a_bucket_with_no_ceiling_says_so_rather_than_reporting_a_zero() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE BUCKET media;").unwrap();

    let described = report(&mut session, "INFO FOR BUCKET media;");
    let Value::Object(fields) = &described else {
        panic!("expected an object, got {described:?}");
    };
    assert_eq!(fields.get("max"), Some(&Value::None));
}

/// A table that is not a bucket is not a bucket to ask about — the refusal
/// `INFO FOR VAULT` and `INFO FOR VECTOR` already make.
///
/// It refuses as **unknown** rather than as a wrong kind, deliberately: the two
/// answers together would tell a caller which names exist without their being
/// able to read either, and the subject that exists to be asked before a listing
/// must not become a way to enumerate.
#[test]
fn info_for_bucket_refuses_a_table_that_holds_no_files() {
    let store = store();
    let mut session = ready(&store);

    let refused = session.run("INFO FOR BUCKET orders;");
    let error = refused.expect_err("a collection is not a bucket");
    let said = error.to_string();
    assert!(
        said.contains("orders") && said.contains("bucket"),
        "the refusal must name the bucket it could not find, and it said: {said}",
    );
}

/// And a name nothing declared refuses the same way, so the two are
/// indistinguishable from outside.
#[test]
fn info_for_bucket_refuses_a_name_nothing_declared() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("INFO FOR BUCKET absent;")
        .expect_err("nothing declared that name");
}
