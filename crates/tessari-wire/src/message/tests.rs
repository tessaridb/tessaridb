#![allow(clippy::panic)]

use tessari_session::{AccessPath, Outcome, Parameters, Plan};
use tessari_types::{Number, RecordId, RecordRef, TableId, Value};

use super::{
    Answer, Correction, Exact, Names, Remark, Request, Suggested, decode_outcome, encode_outcome,
};

/// No answer below carries a reference, so none of them needs a name.
fn unnamed() -> Names {
    Names::new()
}

#[test]
fn a_request_round_trips_with_and_without_credentials() {
    for credentials in [None, Some(("ada".to_owned(), "a long one".to_owned()))] {
        let held = Request {
            script: "SELECT * FROM users;".to_owned(),
            credentials,
            parameters: Parameters::new(),
        };
        let read = Request::decode(&held.encode()).expect("a request");
        assert_eq!(read, held);
    }
}

#[test]
fn a_request_does_not_print_the_password_it_carries() {
    // Nothing prints one today. The line that does is always somewhere else
    // and written later, which is exactly why this is asserted here.
    let held = Request {
        script: "SELECT * FROM users;".to_owned(),
        credentials: Some(("ada".to_owned(), "a long one".to_owned())),
        parameters: Parameters::new(),
    };
    let printed = format!("{held:?}");
    assert!(!printed.contains("a long one"), "{printed}");
    assert!(printed.contains("ada"), "{printed}");
}

#[test]
fn a_request_body_that_promises_more_than_it_holds_is_refused() {
    let mut parameters = Parameters::new();
    parameters.insert("who".to_owned(), Value::String("ada".to_owned()));
    let held = Request {
        script: "SELECT * FROM users WHERE name = $who;".to_owned(),
        credentials: Some(("ada".to_owned(), "x".to_owned())),
        parameters,
    };
    let body = held.encode();
    for cut in 0..body.len() {
        assert!(
            Request::decode(&body[..cut]).is_err(),
            "a cut at {cut} parsed"
        );
    }
}

#[test]
fn every_outcome_shape_round_trips() {
    let outcomes = [
        Outcome::Done,
        Outcome::Value(Value::from("held")),
        Outcome::Value(Value::None),
        Outcome::Keys(vec![RecordId::Int(1), RecordId::Text("a".to_owned())]),
        Outcome::Removed { count: 12_043 },
        Outcome::Records {
            records: vec![(RecordId::Int(7), Value::from("ada"))],
            plan: Plan::new(AccessPath::Index),
            notes: Vec::new(),
            suggestion: None,
            only: false,
        },
    ];
    for outcome in &outcomes {
        let body = encode_outcome(outcome, &unnamed());
        let (answer, used) = decode_outcome(&body, 0).expect("an answer");
        assert_eq!(used, body.len(), "{outcome:?} left bytes unread");
        match (outcome, &answer) {
            (Outcome::Done, Answer::Done)
            | (Outcome::Value(_), Answer::Value { .. })
            | (Outcome::Keys(_), Answer::Keys(_))
            | (Outcome::Removed { .. }, Answer::Removed(_))
            | (Outcome::Records { .. }, Answer::Records { .. }) => {}
            (held, read) => panic!("{held:?} came back as {read:?}"),
        }
    }
}

#[test]
fn a_value_keeps_its_kind_where_json_would_have_flattened_it() {
    // The whole reason this protocol exists rather than the console reading
    // the HTTP endpoint: a decimal is a decimal and not a quoted string a
    // client has to decide about.
    let held = Outcome::Value(Value::Number(Number::Decimal(
        rust_decimal::Decimal::try_from(12.34_f64).expect("a decimal"),
    )));
    let (answer, _) = decode_outcome(&encode_outcome(&held, &unnamed()), 0).expect("an answer");
    match answer {
        Answer::Value {
            value: Value::Number(Number::Decimal(read)),
            ..
        } => {
            assert_eq!(read.to_string(), "12.34");
        }
        other => panic!("a decimal came back as {other:?}"),
    }
}

#[test]
fn a_reference_arrives_with_the_name_a_client_cannot_look_up() {
    // The catalog is on the server. Without this the client renders
    // `<record 3:7>`, which is not something anybody can paste back.
    let table = TableId::new(3);
    let mut names = Names::new();
    names.insert(table, "orders".to_owned());
    let held = Outcome::Records {
        records: vec![(
            RecordId::Int(1),
            Value::Record(RecordRef::new(table, RecordId::Int(7))),
        )],
        plan: Plan::new(AccessPath::Record),
        notes: Vec::new(),
        suggestion: None,
        only: false,
    };
    let (answer, used) = decode_outcome(&encode_outcome(&held, &names), 0).expect("an answer");
    assert_eq!(used, encode_outcome(&held, &names).len());
    match answer {
        Answer::Records { names: read, .. } => {
            assert_eq!(read.get(&table).map(String::as_str), Some("orders"));
        }
        other => panic!("records came back as {other:?}"),
    }
}

#[test]
fn several_outcomes_read_back_in_order_from_one_body() {
    let mut body = Vec::new();
    body.extend_from_slice(&encode_outcome(&Outcome::Done, &unnamed()));
    body.extend_from_slice(&encode_outcome(&Outcome::Removed { count: 3 }, &unnamed()));
    body.extend_from_slice(&encode_outcome(
        &Outcome::Value(Value::from("last")),
        &unnamed(),
    ));

    let (first, at) = decode_outcome(&body, 0).expect("first");
    let (second, at) = decode_outcome(&body, at).expect("second");
    let (third, at) = decode_outcome(&body, at).expect("third");
    assert_eq!(first, Answer::Done);
    assert_eq!(second, Answer::Removed(3));
    assert_eq!(
        third,
        Answer::Value {
            value: Value::from("last"),
            names: unnamed(),
        }
    );
    assert_eq!(at, body.len());
}

/// One read that has something to say, for the compatibility tests below.
fn noted() -> Outcome {
    Outcome::Records {
        records: vec![(RecordId::Int(7), Value::from("ada"))],
        plan: Plan::new(AccessPath::Scan),
        notes: vec![tessari_session::Note::Approximate],
        suggestion: None,
        only: false,
    }
}

#[test]
fn a_note_survives_the_wire() {
    let body = encode_outcome(&noted(), &unnamed());
    let (answer, used) = decode_outcome(&body, 0).expect("an answer");
    assert_eq!(used, body.len());
    let Answer::Records { notes, records, .. } = answer else {
        panic!("not records")
    };
    // The records are still there, which is the half a note must never cost.
    assert_eq!(records.len(), 1);
    assert_eq!(
        notes,
        vec![Remark {
            kind: "approximate".to_owned(),
            message: "an approximate index answered this, so a nearer record may exist".to_owned(),
        }]
    );
}

#[test]
fn a_newer_node_does_not_break_an_older_client() {
    // The older client is modelled by its behaviour rather than by an old
    // build: it reads the fields it knows and then advances by the *declared
    // length*, which is what `decode_outcome` returns. A body carrying notes
    // it never heard of costs it those bytes and not the connection.
    let plain = encode_outcome(
        &Outcome::Records {
            records: vec![(RecordId::Int(7), Value::from("ada"))],
            plan: Plan::new(AccessPath::Scan),
            notes: Vec::new(),
            suggestion: None,
            only: false,
        },
        &unnamed(),
    );
    let noted = encode_outcome(&noted(), &unnamed());
    assert!(noted.len() > plain.len(), "the notes were not encoded");
    // Two outcomes back to back: the second is found only if the first's
    // extra bytes were stepped over correctly, which is the property an
    // appended field actually needs.
    let mut stream = noted.clone();
    stream.extend_from_slice(&plain);
    let (_, after) = decode_outcome(&stream, 0).expect("the first");
    assert_eq!(after, noted.len(), "the notes desynchronised the stream");
    let (second, end) = decode_outcome(&stream, after).expect("the second");
    assert_eq!(end, stream.len());
    assert!(matches!(second, Answer::Records { .. }));
}

#[test]
fn an_older_node_does_not_break_a_newer_client() {
    // The direction the length prefix does *not* cover on its own. An older
    // node sends a body that simply ends after the records, and this build
    // must read that as a node with nothing to say rather than as a short
    // read.
    let full = encode_outcome(
        &Outcome::Records {
            records: vec![(RecordId::Int(7), Value::from("ada"))],
            plan: Plan::new(AccessPath::Scan),
            notes: Vec::new(),
            suggestion: None,
            only: false,
        },
        &unnamed(),
    );
    // Drop the tail the current encoder writes and shrink the declared
    // length to match, which is exactly the body an older node would have
    // produced.
    //
    // The constants are named rather than summed into a literal so that the
    // *reason* for the number survives — but naming them does not make this
    // safe on its own, and the exactness field proved it: appending a field
    // and leaving this alone compiles perfectly and silently retargets the
    // slice at the middle of the tail rather than its start. What actually
    // catches that is the assertion below that the decoded answer has
    // **none** of the appended fields, which fails the moment one of them
    // survives the truncation.
    const NOTE_COUNT: usize = 4;
    const ONLY_FLAG: usize = 1;
    // A tag byte and the four-byte length of an empty reason.
    const EXACTNESS: usize = 1 + 4;
    // One state byte. `None` writes nothing after it, so this is the whole
    // of the field for a read that consulted no dictionary.
    const SUGGESTION: usize = 1;
    let inner = &full[4..full.len() - NOTE_COUNT - ONLY_FLAG - EXACTNESS - SUGGESTION];
    let mut older = Vec::new();
    older.extend_from_slice(&u32::try_from(inner.len()).expect("small").to_be_bytes());
    older.extend_from_slice(inner);
    let (answer, used) = decode_outcome(&older, 0).expect("an older answer");
    assert_eq!(used, older.len());
    let Answer::Records {
        notes,
        records,
        only,
        exact,
        suggestion,
        ..
    } = answer
    else {
        panic!("not records")
    };
    assert_eq!(records.len(), 1, "an older body lost its records");
    assert!(notes.is_empty(), "notes appeared from nowhere");
    assert!(!only, "a body that ends early claimed to be an `ONLY` read");
    // And the one field where absence is **not** its default. A node that
    // predates exactness made no claim about it, and reading the silence as
    // `true` would put a promise in its mouth on the one property whose
    // whole purpose is that it is never inferred.
    assert!(
        exact.is_none(),
        "a body that ends early claimed its answer was exact",
    );
    // And the same again for the newest field, where the mistake would be
    // worse: reading silence as `NotSought` is nearly right and completely
    // unfounded, and reading it as `NothingNearer` would have this client
    // report on a dictionary the older node never had.
    assert!(
        suggestion.is_none(),
        "a body that ends early said something about a suggestion",
    );
}

/// The suggestion's three states survive the wire as three states.
///
/// The encoding gives each its own byte rather than a flag plus a possibly
/// empty list, and this is what holds it to that: an implementation that
/// wrote `NothingNearer` as an empty `DidYouMean` would pass every test
/// about corrections and fail here, which is the right place for it to fail
/// because the difference is the whole point of the field.
#[test]
fn a_suggestion_crosses_the_wire_as_three_states_and_not_two() {
    let crossed = |suggestion| {
        let encoded = encode_outcome(
            &Outcome::Records {
                records: vec![(RecordId::Int(7), Value::from("ada"))],
                plan: Plan::new(AccessPath::Scan),
                notes: Vec::new(),
                suggestion,
                only: false,
            },
            &unnamed(),
        );
        let (answer, used) = decode_outcome(&encoded, 0).expect("a suggestion");
        assert_eq!(
            used,
            encoded.len(),
            "the suggestion desynchronised the stream"
        );
        let Answer::Records { suggestion, .. } = answer else {
            panic!("not records")
        };
        suggestion
    };

    assert_eq!(crossed(None), Some(Suggested::NotSought));
    assert_eq!(
        crossed(Some(tessari_session::Suggestion::NothingNearer)),
        Some(Suggested::NothingNearer),
        "a dictionary that was asked crossed as one that was not"
    );
    assert_eq!(
        crossed(Some(tessari_session::Suggestion::DidYouMean(vec![
            tessari_session::Nearest {
                typed: "vecter".to_owned(),
                instead: "vector".to_owned(),
            }
        ]))),
        Some(Suggested::DidYouMean(vec![Correction {
            typed: "vecter".to_owned(),
            instead: "vector".to_owned(),
        }])),
    );
}

/// The three states, told apart.
///
/// A `bool` would collapse the first two of these into each other at the
/// first `unwrap_or`, which is why the client's field is an `Option` around a
/// two-state type rather than an `Option<bool>` — and why this test asserts
/// all three rather than the interesting one.
#[test]
fn a_node_that_says_nothing_is_not_a_node_that_says_exact() {
    let said = |access| {
        let encoded = encode_outcome(
            &Outcome::Records {
                records: vec![(RecordId::Int(7), Value::from("ada"))],
                plan: Plan::new(access),
                notes: Vec::new(),
                suggestion: None,
                only: false,
            },
            &unnamed(),
        );
        let (answer, used) = decode_outcome(&encoded, 0).expect("an answer");
        assert_eq!(used, encoded.len(), "exactness desynchronised the stream");
        let Answer::Records { exact, .. } = answer else {
            panic!("not records")
        };
        exact
    };

    assert_eq!(said(AccessPath::Scan), Some(Exact::Yes));
    let Some(Exact::No { reason }) = said(AccessPath::Approximate) else {
        panic!("the graph walk crossed the wire calling itself exact");
    };
    // The node's own words, carried rather than re-invented on this side: a
    // client that had to phrase the reason itself would be describing a read
    // it did not perform.
    assert_eq!(reason, tessari_session::Note::Approximate.message());
}

#[test]
fn the_only_flag_survives_the_wire_and_does_not_desynchronise_a_stream() {
    let alone = encode_outcome(
        &Outcome::Records {
            records: vec![(RecordId::Int(7), Value::from("ada"))],
            plan: Plan::new(AccessPath::Record),
            notes: Vec::new(),
            suggestion: None,
            only: true,
        },
        &unnamed(),
    );
    let (answer, used) = decode_outcome(&alone, 0).expect("an answer");
    assert_eq!(used, alone.len(), "the flag left bytes unread");
    assert!(
        matches!(answer, Answer::Records { only: true, .. }),
        "the flag did not survive"
    );
    // Back to back with a second outcome, which is the property an appended
    // field actually needs: the second is found only if the first's flag was
    // stepped over.
    let mut stream = alone.clone();
    stream.extend_from_slice(&alone);
    let (_, after) = decode_outcome(&stream, 0).expect("the first");
    assert_eq!(after, alone.len(), "the flag desynchronised the stream");
    let (second, end) = decode_outcome(&stream, after).expect("the second");
    assert_eq!(end, stream.len());
    assert!(matches!(second, Answer::Records { only: true, .. }));
}
