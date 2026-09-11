//! The users who already exist keep exactly what they had.
//!
//! # Why this is a test and not a note in a changelog
//!
//! Because the model this store just gained exists to make one bundle
//! **refusable** — `{read, write, manage}`, the editor's — and the tidy thing to
//! do next would be to stop handing that bundle out to the editors who already
//! have it. That would be an outage delivered as a migration: every one of them
//! was declared under a promise that they may define structure, and a store that
//! narrows them on upgrade breaks working systems at the moment their operator
//! is least expecting it. The rule governs what can now be **said**; it does not
//! reach back.
//!
//! So the mapping deliberately keeps a combination the new rule forbids, and
//! this file is the proof that it does — a table of every role at every reach,
//! asserted twice: once as the set the catalog reports, and once as the
//! behaviour a holder of that role still gets.
//!
//! The second half is the one that matters. A set can be reported correctly and
//! still be enforced differently, and "the authorities look right" is exactly
//! the kind of claim that passes while a statement somebody depends on has
//! started being refused.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";

/// What each ladder role stood for, written from the ladder rather than from
/// `Held::from_role` — the second source is the point, exactly as it is in the
/// differential. `@R` is the reach the user was declared at.
///
/// # `owner` is written with a sixth entry the ladder never had
///
/// And that is the one edit this table is allowed. The ladder's `owner` meant
/// *everything*, so a kind added to the closed set joins it — which widens every
/// owner a migrated store already holds. This assertion is what stops that
/// happening in silence: it fired when `replicate` arrived, and the decision
/// behind adding the entry rather than carving the kind out of the bundle is
/// recorded in `Held::from_role`. Narrowing a role on upgrade is an outage;
/// widening one is an escalation; both have to be somebody's decision, and this
/// table is where they are made to be.
const LADDER: [(&str, &[&str]); 3] = [
    ("viewer", &["read@R"]),
    ("editor", &["manage@R", "read@R", "write@R"]),
    (
        "owner",
        &[
            "govern@R",
            "manage@R",
            "operate@R",
            "read@R",
            "replicate@R",
            "write@R",
        ],
    ),
];

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

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
        .map(|one| {
            let Value::Object(fields) = one else {
                panic!("expected an object per authority");
            };
            match (fields.get("authority"), fields.get("reach")) {
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

/// A store with a tenancy and a store owner to declare the rest.
fn peopled(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders; CREATE orders:1 = { total: 5 };\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    root
}

#[test]
fn every_ladder_role_keeps_exactly_the_authorities_it_stood_for() {
    let store = store();
    let mut root = peopled(&store);

    // Every role at every reach the ladder could be declared at. `store` is
    // included because it is the one where the owner's bundle is widest and so
    // the one where a narrowing would do the most damage.
    for (role, expected) in LADDER {
        for (suffix, clause, reach) in [
            ("s", String::new(), "store"),
            ("n", "ON NAMESPACE prod ".to_owned(), "prod"),
            ("d", "ON prod.shop ".to_owned(), "prod.shop"),
        ] {
            let name = format!("{role}_{suffix}");
            root.run(&format!(
                "DEFINE USER {name} {clause}ROLE {role} PASSWORD '{PASSWORD}';"
            ))
            .unwrap();
            let mut want: Vec<String> = expected
                .iter()
                .map(|entry| entry.replace("@R", &format!("@{reach}")))
                .collect();
            want.sort();
            assert_eq!(
                held(&mut root, &name),
                want,
                "{name} lost or gained something"
            );
        }
    }
}

#[test]
fn an_editor_still_defines_structure_and_a_viewer_still_cannot() {
    // The behavioural half. The set can be reported correctly and enforced
    // differently, and an editor who quietly stopped being able to run
    // `DEFINE TABLE` is precisely the outage this mapping exists to prevent.
    let store = store();
    let mut root = peopled(&store);
    for role in ["viewer", "editor", "owner"] {
        root.run(&format!(
            "DEFINE USER {role}_d ON prod.shop ROLE {role} PASSWORD '{PASSWORD}';"
        ))
        .unwrap();
    }

    // What the ladder promised, statement by statement, written out rather than
    // computed from the new model — a table computed from the thing under test
    // would agree with it by construction.
    //
    // `%R%` in a name is replaced by the role running it, because a declaring
    // statement that succeeds for the editor and then collides for the owner
    // fails on a name and reads exactly like a refusal. The placeholder is not
    // the word `ROLE`, which is a keyword one of these statements needs.
    let promised: [(&str, [bool; 3]); 4] = [
        ("SELECT * FROM orders;", [true, true, true]),
        (
            "CREATE orders:'new%R%' = { total: 1 };",
            [false, true, true],
        ),
        (
            "DEFINE TABLE shipments%R% (code string);",
            [false, true, true],
        ),
        (
            "DEFINE USER extra%R% ON prod.shop ROLE viewer PASSWORD 'correct horse battery';",
            [false, false, true],
        ),
    ];

    for (statement, allowed) in promised {
        for (index, role) in ["viewer", "editor", "owner"].into_iter().enumerate() {
            let mut session = Session::new(&store);
            session.sign_in(&format!("{role}_d"), PASSWORD).unwrap();
            let answered = session.run(&format!(
                "USE NAMESPACE prod; USE DATABASE shop; {}",
                statement.replace("%R%", role)
            ));
            assert_eq!(
                answered.is_ok(),
                allowed[index],
                "{role} and {statement:?}: the ladder promised {}, the store {}{}",
                if allowed[index] { "yes" } else { "no" },
                if answered.is_ok() { "yes" } else { "no" },
                answered
                    .err()
                    .map_or(String::new(), |why| format!(" ({why})")),
            );
        }
    }
}
