//! Every demand class, refused by the one kind it is missing.
//!
//! `authorities.rs` proves the four rules one case at a time. This file asks
//! the question the other way round, over the classes the statement table maps
//! to: for each class, one statement, and for each kind that class demands, a
//! user holding every kind *except* that one. The refusal has to name the
//! missing kind and nothing else will do.
//!
//! Why a whole-table pass and not more cases: with a set rather than a rank, an
//! arm carrying too few kinds is a silent privilege escalation, and `{write}`
//! type-checks exactly like `{read, write}`. The exhaustive match guards against
//! a missing arm; only a pass that removes each demanded kind in turn guards
//! against a thin one.
//!
//! The two consumer classes (`manage, write` and `manage, read, write`) are held
//! by `consumers.rs`, which already declares a broker to declare one against.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::Store;

const PASSWORD: &str = "correct horse battery";

/// The five kinds a user can be declared with.
const KINDS: [&str; 5] = ["read", "write", "manage", "govern", "operate"];

/// One statement per demand class, and the kinds that class demands.
///
/// Each is run inside `prod.shop` by a store-wide user, so a refusal can only
/// come from the kind and never from the reach.
const CLASSES: [(&str, &[&str]); 9] = [
    ("SELECT * FROM orders;", &["read"]),
    ("UPSERT orders:1 = { total: 1 };", &["write"]),
    ("CREATE orders:2 = { total: 1 };", &["read", "write"]),
    ("DEFINE COLLECTION extra;", &["manage"]),
    ("DEFINE NAMESPACE other;", &["manage"]),
    ("DROP USER nobody;", &["govern"]),
    ("INFO FOR NODE;", &["operate"]),
    ("BACKUP;", &["read", "operate"]),
    (
        "RESTORE SCRIPT FROM 'nothing.tessariql';",
        &["manage", "write", "operate"],
    ),
];

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A closed store with `prod.shop.orders`, and one store-wide user per kind,
/// each holding the other four.
fn without_one_kind_each(store: &Store) {
    let mut root = Session::new(store);
    root.run(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
         DEFINE DATABASE shop; USE DATABASE shop;\n\
         DEFINE TABLE orders SCHEMALESS;\n\
         DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
    )
    .unwrap();
    let mut root = working(store, "root");
    for lacking in KINDS {
        let held: Vec<&str> = KINDS.into_iter().filter(|kind| *kind != lacking).collect();
        root.run(&format!(
            "DEFINE USER no_{lacking} AUTHORITIES {} PASSWORD '{PASSWORD}';",
            held.join(", ")
        ))
        .unwrap();
    }
}

/// A session signed in and selected onto `prod.shop`.
fn working<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

#[test]
fn each_demand_class_is_refused_by_name_when_any_one_of_its_kinds_is_missing() {
    let store = store();
    without_one_kind_each(&store);

    let mut checked = 0_usize;
    for (statement, demanded) in CLASSES {
        for lacking in demanded {
            let mut session = working(&store, &format!("no_{lacking}"));
            match session.run(statement) {
                Err(Error::RoleForbids { needs, .. }) => assert_eq!(
                    needs, *lacking,
                    "{statement} refused no_{lacking} for the wrong kind"
                ),
                other => {
                    panic!("{statement} run by no_{lacking}: expected RoleForbids, got {other:?}")
                }
            }
            checked = checked.saturating_add(1);
        }
    }
    // Derived from the table rather than typed, so a row added above is a row
    // checked here.
    let expected: usize = CLASSES.iter().map(|(_, demanded)| demanded.len()).sum();
    assert_eq!(checked, expected);
}

#[test]
fn a_kind_a_class_does_not_demand_is_never_the_reason_it_is_refused() {
    // The other half, without which the test above passes against a store that
    // refuses everybody everything: a user missing a kind the statement does
    // not demand is not refused for authority at all. It may still be refused
    // for something else — `RESTORE` has no file to read — and that is fine;
    // what may not happen is `RoleForbids`.
    let store = store();
    without_one_kind_each(&store);

    for (statement, demanded) in CLASSES {
        for lacking in KINDS.into_iter().filter(|kind| !demanded.contains(kind)) {
            let mut session = working(&store, &format!("no_{lacking}"));
            if let Err(Error::RoleForbids { needs, .. }) = session.run(statement) {
                panic!("{statement} refused no_{lacking} for {needs}, which it does not demand");
            }
        }
    }
}

#[test]
fn a_store_wide_class_is_refused_to_a_user_holding_one_database_whatever_they_hold_there() {
    // The reach half of the store-wide classes. An owner of `prod.shop` holds all
    // five kinds there, so the kind check passes and only the reach can refuse.
    let store = store();
    without_one_kind_each(&store);
    working(&store, "root")
        .run(&format!(
            "DEFINE USER shop_owner ON prod.shop ROLE owner PASSWORD '{PASSWORD}';"
        ))
        .unwrap();

    for statement in [
        "DEFINE NAMESPACE other;",
        "INFO FOR NODE;",
        "BACKUP;",
        "RESTORE SCRIPT FROM 'nothing.tessariql';",
    ] {
        let mut session = working(&store, "shop_owner");
        match session.run(statement) {
            Err(Error::NotTheWholeStore { .. }) => {}
            other => panic!(
                "{statement} by a one-database owner: expected NotTheWholeStore, got {other:?}"
            ),
        }
    }
}
