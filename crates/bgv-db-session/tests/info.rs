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

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::{Outcome, Session};
use bgv_db_storage::Store;
use bgv_db_types::Value;

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
             DEFINE TABLE orders;\n\
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

    session.run("DEFINE TABLE audit;").unwrap();
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
    assert!(refused.to_string().contains("administer"), "{refused}");
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
