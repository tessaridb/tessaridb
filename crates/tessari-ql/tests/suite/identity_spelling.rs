//! What the store answers with is what the grammar reads back.
//!
//! # The promise being held here
//!
//! A record identity leaves the store as text, and the protocol says a client
//! naming that record "writes that text into its next script". That sentence is
//! only true if the text stands where the grammar puts an identity — and for two
//! of the four kinds it did not, silently, because the form the store answered
//! with was a rendering meant for log lines.
//!
//! The failure had no shape anybody would recognise. A UUID identity came back
//! as thirty-two undivided hex digits, and `users:0195e0…` does not fail as
//! *"that is not an identity"* — it lexes as a number with a suffix and is
//! refused as **"not a duration this store can hold"**. A person reading that
//! looks for a duration.
//!
//! # Why it is asserted per kind rather than once
//!
//! Because the kind that worked is the kind every example uses. An integer
//! identity round-tripped from the beginning and would have carried a single
//! assertion, a hand-written fixture and a green suite all the way to a caller
//! with a `uuid` table. The four kinds are enumerated here so that the one being
//! added next has to be decided about instead of inherited.

#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_ql::{CreateTarget, Identity, Source, StatementKind, parse};
use tessari_types::RecordId;

/// Every kind a record identity has. `docs/key-grammar.md` §5 fixes the set.
fn every_kind() -> Vec<(&'static str, RecordId)> {
    vec![
        ("int", RecordId::Int(42)),
        ("int, negative", RecordId::Int(-7)),
        (
            "int, the largest one a record can hold",
            RecordId::Int(i64::MAX),
        ),
        ("text", RecordId::from("ada")),
        // The two a naive quoting would lose, and the reason the escaper is
        // shared with the value renderer rather than written twice.
        (
            "text holding the quote that would end it",
            RecordId::from("it's"),
        ),
        (
            "text holding a backslash and a newline",
            RecordId::from("a\\b\nc"),
        ),
        // A text identity that reads as another kind if it is left unquoted:
        // without the quotes this is the *integer* 1, which is a different
        // record, and the value system says so out loud.
        ("text that would read as an integer", RecordId::from("1")),
        ("text that would read as a keyword", RecordId::from("uuid")),
        (
            "uuid",
            RecordId::Uuid([
                0x01, 0x95, 0xe0, 0xab, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0xa, 0xb,
            ]),
        ),
        // Leading zeroes are where a UUID and a shorter thing that looks like
        // one stop being distinguishable if a digit is dropped.
        ("uuid, all zero", RecordId::Uuid([0; 16])),
        ("uuid, all ones", RecordId::Uuid([0xff; 16])),
        ("bytes", RecordId::Bytes(vec![0x0a, 0x1b])),
    ]
}

/// The identity a statement addressing `users:<spelling>` resolved to.
fn addressed(spelling: &str) -> RecordId {
    let source = format!("SELECT * FROM users:{spelling};");
    let parsed = match parse(&source) {
        Ok(parsed) => parsed,
        Err(error) => panic!("{source}\n  the store's own spelling did not parse: {error}"),
    };
    let statement = parsed.statements.into_iter().next().expect("one statement");
    let StatementKind::Select(select) = statement.kind else {
        panic!("{source}: expected a select");
    };
    match select.from {
        Source::Record(target) => match target.id {
            Identity::Fixed(id) => id,
            other => panic!("{source}: expected a fixed identity, got {other:?}"),
        },
        other => panic!("{source}: expected a read of one record, got {other:?}"),
    }
}

#[test]
fn every_identity_the_store_can_answer_with_reads_back_as_itself() {
    for (what, id) in every_kind() {
        let spelled = id.to_literal();
        let read = addressed(&spelled);
        assert_eq!(
            read, id,
            "{what}: `users:{spelled}` came back as a different record"
        );
    }
}

#[test]
fn a_created_record_can_be_addressed_by_the_spelling_it_was_created_with() {
    // The same claim from the write side, because that is the direction the
    // caller actually travels: the store answers a generated `CREATE` with an
    // identity, and the next thing the caller writes is a statement holding it.
    for (what, id) in every_kind() {
        let spelled = id.to_literal();
        let source = format!("CREATE users:{spelled} = {{ name: 'ada' }};");
        let parsed = match parse(&source) {
            Ok(parsed) => parsed,
            Err(error) => panic!("{what}: {source}\n  failed: {error}"),
        };
        let statement = parsed.statements.into_iter().next().expect("one statement");
        let StatementKind::Create { target, .. } = statement.kind else {
            panic!("{what}: expected a create");
        };
        let CreateTarget::Named(target) = target else {
            panic!("{what}: an addressed create should name the record");
        };
        assert_eq!(target.id, Identity::Fixed(id), "{what}");
    }
}

#[test]
fn the_spelling_is_not_the_rendering_and_the_difference_is_the_point() {
    // Pinned as a pair so that a future edit collapsing the two has to delete an
    // assertion that says why they are two.
    let uuid = RecordId::Uuid([0; 16]);
    assert_eq!(uuid.to_string(), "0".repeat(32));
    assert_eq!(
        uuid.to_literal(),
        "uuid '00000000-0000-0000-0000-000000000000'"
    );
    // And the rendering does not stand where an identity stands — in two
    // different ways, of which the one that looks milder is the worse.
    //
    // A uuid holding any letter is refused outright, because the digits do not
    // lex as anything an identity may be.
    assert!(
        parse("SELECT * FROM users:0195e0ab000102030405060708090a0b;").is_err(),
        "a uuid's log rendering should not stand where an identity stands"
    );
    // The nil uuid is not refused at all. Thirty-two zeros are a perfectly good
    // integer literal, so the statement is accepted and quietly addresses
    // `users:0` — a different record, with nothing anywhere in an error state.
    // That is the same defect as the text identity below, arrived at from the
    // other end, and it is why this assertion is about *which record* rather
    // than about parsing: an earlier draft asserted the rendering failed to
    // parse, which is true of every uuid except the one whose failure is silent.
    assert_eq!(
        addressed(&uuid.to_string()),
        RecordId::Int(0),
        "the nil uuid's rendering addresses another record rather than failing"
    );

    let text = RecordId::from("1");
    assert_eq!(text.to_string(), "1");
    assert_eq!(text.to_literal(), "'1'");
    // Worse than not parsing: it parses, as a different record.
    assert_ne!(addressed(&text.to_string()), text);
    assert_eq!(addressed(&text.to_literal()), text);
}
