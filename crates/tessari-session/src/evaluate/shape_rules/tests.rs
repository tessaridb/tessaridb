#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_ql::{Select, StatementKind};

use super::travelling_order;

/// The read a statement is.
fn read(statement: &str) -> Select {
    let mut script = tessari_ql::parse(statement).unwrap();
    match script.statements.remove(0).kind {
        StatementKind::Select(select) => *select,
        other => panic!("a read, got {other:?}"),
    }
}

#[test]
fn an_order_with_the_absences_moved_is_not_handed_to_the_leaders() {
    // A leader ranks in the declared order, so its first `n` under a moved
    // `NULLS` could miss records the answer holds (ADR-0134 D8).
    let all = None;
    assert!(travelling_order(&read("SELECT * FROM t ORDER BY due LIMIT 2;"), &all).is_some());
    assert!(
        travelling_order(
            &read("SELECT * FROM t ORDER BY due NULLS FIRST LIMIT 2;"),
            &all
        )
        .is_some()
    );
    assert!(
        travelling_order(
            &read("SELECT * FROM t ORDER BY due NULLS LAST LIMIT 2;"),
            &all
        )
        .is_none()
    );
    assert!(
        travelling_order(
            &read("SELECT * FROM t ORDER BY due DESC NULLS FIRST LIMIT 2;"),
            &all
        )
        .is_none()
    );
}
