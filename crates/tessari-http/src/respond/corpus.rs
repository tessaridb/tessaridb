//! The published outcome renderings, held against this build's renderer.
//!
//! # What this checks, and why it is not the Python verifier
//!
//! `tessaridb-protocol/conformance/json-v1.json` carries two sets. Its 59
//! **value** cases are already driven against a running node by that
//! repository's `verify_json_against_node.py`. Its 20 **outcome** cases —
//! the shape of the object a caller parses — were, when this was written, read
//! by nothing anywhere: not by that script, which iterates `cases` alone, not by
//! the SDK, and not here.
//!
//! Those are the cases that fail silently. `{"kind":"value"}` and
//! `{"kind":"value","value":null}` are different answers; a `suggestion` key
//! that is absent and one holding `{"corrections":[]}` are different claims; a
//! plan without `exact` is a node that made no claim at all. Each pair renders
//! identically to a client that reaches for a default, which is the argument the
//! corpus was built on and the reason a renderer nobody compares to it drifts
//! without anything going red.
//!
//! # The corpus is the oracle in one direction only
//!
//! It is generated from `spec/protocol-v1.md` by a second implementation that
//! never reads this code, so a disagreement is a finding either way round: the
//! document may be wrong about the engine, or the engine may be wrong about the
//! document. This corpus has been wrong about this engine before — it claimed a
//! `keys` entry was written `table:id` when a node has only ever answered the id
//! half. So a failure here is read, not patched on whichever side is nearer.
//!
//! # An unreachable case is an output, never a skip
//!
//! Two of the twenty describe a node this build is not: one carries a kind no
//! variant of [`Outcome`] produces, and one a plan with no `exact` key, which
//! `Plan::to_value` writes unconditionally and on purpose. They are named with
//! their reason and counted separately. A run that quietly skipped them would
//! print the same "all verified" as a run that checked everything.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tessari_session::Plan;
use tessaridb::{AccessPath, Nearest, Note, Number, Outcome, RecordId, Suggestion, Value};

use super::{encode, json};

/// A JSON value, read back so the comparison is structural.
///
/// Neither §5.6 nor §5.7 makes key order or whitespace normative, and the two
/// sides genuinely disagree about order — this renderer writes a plan's keys
/// sorted, because a plan is a `BTreeMap`, while the corpus writes them in the
/// order the specification introduces them. Comparing the text would fail on a
/// difference nobody promised.
#[derive(Debug, PartialEq, Eq)]
enum Json {
    Null,
    Bool(bool),
    /// Kept as written: the corpus holds integers only, and re-reading them as
    /// a float would invent a precision question the document does not have.
    Number(String),
    Text(String),
    Array(Vec<Json>),
    /// Sorted, which is what makes equality of two of these structural.
    Object(BTreeMap<String, Json>),
}

/// Read one value, answering it and where the scan stopped.
///
/// A total function over its input: `None` is malformed, and the caller says so
/// with the position. Only ever pointed at a committed corpus, so `None` means
/// the corpus is broken rather than that this reader met something exotic.
fn read(text: &[u8], mut at: usize) -> Option<(Json, usize)> {
    while at < text.len() && text[at].is_ascii_whitespace() {
        at = at.saturating_add(1);
    }
    match *text.get(at)? {
        b'{' => {
            let mut fields = BTreeMap::new();
            at = at.saturating_add(1);
            loop {
                while at < text.len() && text[at].is_ascii_whitespace() {
                    at = at.saturating_add(1);
                }
                if *text.get(at)? == b'}' {
                    return Some((Json::Object(fields), at.saturating_add(1)));
                }
                if text[at] == b',' {
                    at = at.saturating_add(1);
                    continue;
                }
                let (name, next) = read(text, at)?;
                let Json::Text(name) = name else { return None };
                at = next;
                while at < text.len() && text[at].is_ascii_whitespace() {
                    at = at.saturating_add(1);
                }
                if *text.get(at)? != b':' {
                    return None;
                }
                let (value, next) = read(text, at.saturating_add(1))?;
                fields.insert(name, value);
                at = next;
            }
        }
        b'[' => {
            let mut items = Vec::new();
            at = at.saturating_add(1);
            loop {
                while at < text.len() && text[at].is_ascii_whitespace() {
                    at = at.saturating_add(1);
                }
                if *text.get(at)? == b']' {
                    return Some((Json::Array(items), at.saturating_add(1)));
                }
                if text[at] == b',' {
                    at = at.saturating_add(1);
                    continue;
                }
                let (item, next) = read(text, at)?;
                items.push(item);
                at = next;
            }
        }
        b'"' => {
            let mut out = String::new();
            at = at.saturating_add(1);
            loop {
                match *text.get(at)? {
                    b'"' => return Some((Json::Text(out), at.saturating_add(1))),
                    b'\\' => {
                        // Kept as written rather than resolved. Both sides of
                        // the comparison go through this reader, and the corpus
                        // escapes nothing this renderer does not.
                        out.push('\\');
                        out.push(char::from(*text.get(at.saturating_add(1))?));
                        at = at.saturating_add(2);
                    }
                    _ => {
                        let start = at;
                        while at < text.len() && text[at] != b'"' && text[at] != b'\\' {
                            at = at.saturating_add(1);
                        }
                        out.push_str(std::str::from_utf8(text.get(start..at)?).ok()?);
                    }
                }
            }
        }
        // Matched whole rather than by their first byte. A reader that took
        // `t` for `true` would take it from either side of the comparison, and
        // two sides agreeing through the same wrong reading is the one failure
        // a reader used as an oracle must not have.
        b't' | b'f' | b'n' => {
            for (word, value) in [
                ("true", Json::Bool(true)),
                ("false", Json::Bool(false)),
                ("null", Json::Null),
            ] {
                if text.get(at..at.saturating_add(word.len())) == Some(word.as_bytes()) {
                    return Some((value, at.saturating_add(word.len())));
                }
            }
            None
        }
        _ => {
            let start = at;
            while at < text.len() && !b",}] \n\t\r".contains(&text[at]) {
                at = at.saturating_add(1);
            }
            Some((
                Json::Number(std::str::from_utf8(text.get(start..at)?).ok()?.to_owned()),
                at,
            ))
        }
    }
}

/// Where the corpus lives.
///
/// The protocol is its own repository, so the directory is configurable and the
/// default assumes it sits beside this one — the same rule, and the same
/// variable, the Rust SDK already uses. A missing corpus **fails** rather than
/// skipping: a check that passes having found nothing reports coverage it does
/// not have.
fn corpus() -> Json {
    let path = std::env::var("TESSARI_PROTOCOL_CONFORMANCE").map_or_else(
        |_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../tessaridb-protocol/conformance")
        },
        PathBuf::from,
    );
    let path = path.join("json-v1.json");
    let raw = std::fs::read(&path).map_err(|why| format!("{}: {why}", path.display()));
    let raw = raw.expect(
        "the published corpus is required, not optional — a check that passes \
         having found nothing reports coverage it does not have. Clone \
         tessaridb-protocol beside this repository, or point \
         TESSARI_PROTOCOL_CONFORMANCE at its conformance directory",
    );
    read(&raw, 0).expect("the corpus is JSON").0
}

/// Build the reachable case, boxing the outcome the enum carries.
fn is(outcome: Outcome) -> Rendering {
    Rendering::Is(Box::new(outcome))
}

/// What this build does with a named case.
enum Rendering {
    /// The outcome that must render as the corpus says.
    ///
    /// Boxed: an `Outcome` carries a record list and a plan, and the other
    /// variant is a `&'static str`.
    Is(Box<Outcome>),
    /// A node this build is not, and why.
    NotThisBuild(&'static str),
}

/// The record `{"name":"ada"}`, which four cases carry.
fn ada() -> Value {
    Value::Object(BTreeMap::from([("name".to_owned(), Value::from("ada"))]))
}

/// The outcome a case names, or the reason this build cannot produce one.
///
/// By name rather than by interpreting the corpus's abstract `outcome` field:
/// the `json` field is the oracle and this is only the input to it, so reading
/// the other field would be writing a second value codec to test a renderer. A
/// name the corpus adds and this list does not know fails, which is what keeps
/// the two in step.
fn rendering(name: &str) -> Option<Rendering> {
    let records = |plan: Plan| Outcome::Records {
        records: Vec::new(),
        plan,
        notes: Vec::new(),
        suggestion: None,
        only: false,
    };
    let suggested = |plan: Plan, suggestion: Suggestion| Outcome::Records {
        records: Vec::new(),
        plan,
        notes: Vec::new(),
        suggestion: Some(suggestion),
        only: false,
    };
    let outcome = match name {
        "outcome-done" => is(Outcome::Done),
        "outcome-value" => is(Outcome::Value(Value::Number(Number::Integer(4)))),
        "outcome-value-none-has-no-value-key" => is(Outcome::Value(Value::None)),
        "outcome-value-null-has-one" => is(Outcome::Value(Value::Null)),
        "outcome-removed" => is(Outcome::Removed { count: 12_043 }),
        "outcome-removed-zero" => is(Outcome::Removed { count: 0 }),
        "outcome-unknown" => Rendering::NotThisBuild(
            "`unknown` is what the wildcard arm answers for a kind this binary \
             does not know, and every variant of `Outcome` is known here. Only \
             a client reading an older node's answer meets it",
        ),
        "outcome-keys-is-an-array-of-strings" => is(Outcome::Keys(vec![
            RecordId::Int(1),
            RecordId::Text("ada".to_owned()),
        ])),
        "outcome-keys-empty" => is(Outcome::Keys(Vec::new())),
        "outcome-records-element-is-a-pair" => is(Outcome::Records {
            records: vec![(RecordId::Int(1), ada())],
            plan: Plan::new(AccessPath::Record).on("users"),
            notes: Vec::new(),
            suggestion: None,
            only: false,
        }),
        "outcome-records-nothing-to-report" => {
            let mut plan = Plan::new(AccessPath::Scan).on("users");
            plan.cells = Some(40);
            is(records(plan))
        }
        "outcome-records-with-a-note" => {
            let mut plan = Plan::new(AccessPath::Scan).on("users");
            plan.cells = Some(40);
            is(Outcome::Records {
                records: Vec::new(),
                plan,
                notes: vec![Note::FellBack {
                    from: AccessPath::Index,
                    to: AccessPath::Scan,
                }],
                suggestion: None,
                only: false,
            })
        }
        "outcome-records-only" => {
            let mut plan = Plan::new(AccessPath::Index).on("users");
            plan.index = Some("by_name".to_owned());
            plan.shape = Some("point");
            is(Outcome::Records {
                records: vec![(RecordId::Int(1), ada())],
                plan,
                notes: Vec::new(),
                suggestion: None,
                only: true,
            })
        }
        "outcome-records-plan-omits-what-it-does-not-know" => {
            is(records(Plan::new(AccessPath::Scan)))
        }
        "outcome-records-plan-says-why-it-is-not-exact" => {
            let mut plan = Plan::new(AccessPath::Approximate).on("points");
            plan.index = Some("by_at".to_owned());
            is(records(plan))
        }
        "outcome-records-plan-without-exactness-said-nothing" => Rendering::NotThisBuild(
            "`Plan::to_value` writes `exact` on every plan, deliberately and \
             including when it is true — a field that appears only when it is \
             interesting teaches a reader that its absence means the dull \
             value, and here the dull value is a claim. Only a node predating \
             the field omits it",
        ),
        "outcome-records-suggestion-not-sought" => {
            is(records(Plan::new(AccessPath::Scan).on("users")))
        }
        "outcome-records-suggestion-nothing-nearer" => {
            let mut plan = Plan::new(AccessPath::Index).on("users");
            plan.index = Some("by_body".to_owned());
            is(suggested(plan, Suggestion::NothingNearer))
        }
        "outcome-records-suggestion-did-you-mean" => {
            let mut plan = Plan::new(AccessPath::Index).on("notes");
            plan.index = Some("by_body".to_owned());
            is(suggested(
                plan,
                Suggestion::DidYouMean(vec![Nearest {
                    typed: "vecter".to_owned(),
                    instead: "vector".to_owned(),
                }]),
            ))
        }
        "outcome-records-suggestion-beside-records-that-answered" => {
            let mut plan = Plan::new(AccessPath::Index).on("notes");
            plan.index = Some("by_body".to_owned());
            is(Outcome::Records {
                records: vec![(
                    RecordId::Int(3),
                    Value::Object(BTreeMap::from([(
                        "body".to_owned(),
                        Value::from("the engine stores every vector"),
                    )])),
                )],
                plan,
                notes: Vec::new(),
                suggestion: Some(Suggestion::DidYouMean(vec![Nearest {
                    typed: "vecter".to_owned(),
                    instead: "vector".to_owned(),
                }])),
                only: false,
            })
        }
        _ => return None,
    };
    Some(outcome)
}

/// A named field of an object, when it is one.
fn field<'a>(value: &'a Json, name: &str) -> Option<&'a Json> {
    match value {
        Json::Object(fields) => fields.get(name),
        _ => None,
    }
}

/// The items of an array, when it is one.
fn items(value: &Json) -> Option<&[Json]> {
    match value {
        Json::Array(items) => Some(items),
        _ => None,
    }
}

/// The text of a string, when it is one.
fn text(value: &Json) -> Option<&str> {
    match value {
        Json::Text(text) => Some(text),
        _ => None,
    }
}

#[test]
fn every_published_outcome_rendering_is_the_one_this_build_writes() {
    let corpus = corpus();
    let outcomes = field(&corpus, "outcomes")
        .and_then(items)
        .expect("the corpus carries an `outcomes` array");
    assert!(
        !outcomes.is_empty(),
        "an empty corpus verifies nothing while reporting that it did"
    );

    let mut verified = 0_usize;
    let mut unreachable: Vec<(&str, &'static str)> = Vec::new();
    let mut unknown: Vec<&str> = Vec::new();
    let mut wrong: Vec<String> = Vec::new();

    for case in outcomes {
        let name = field(case, "name")
            .and_then(text)
            .expect("every outcome case is named");
        let Some(expected) = field(case, "json") else {
            wrong.push(format!("{name}\n  the corpus case carries no rendering"));
            continue;
        };
        match rendering(name) {
            None => unknown.push(name),
            Some(Rendering::NotThisBuild(why)) => unreachable.push((name, why)),
            Some(Rendering::Is(outcome)) => {
                let mut body = String::new();
                encode(&mut body, &outcome, &json::Names::new());
                match read(body.as_bytes(), 0) {
                    Some((rendered, _)) if &rendered == expected => {
                        verified = verified.saturating_add(1)
                    }
                    _ => wrong.push(format!(
                        "{name}\n  this build: {body}\n  the corpus: {expected:?}"
                    )),
                }
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "this build and the published corpus disagree. Neither side is the \
         oracle by default — the corpus is generated from the specification by \
         an implementation that never reads this code, so read both before \
         changing either:\n{}",
        wrong.join("\n")
    );
    assert!(
        unknown.is_empty(),
        "the corpus carries outcome cases this check does not know, so they \
         are verified against nothing: {unknown:?}"
    );
    assert_eq!(
        verified.saturating_add(unreachable.len()),
        outcomes.len(),
        "every published outcome reaches a terminal status"
    );
    println!(
        "outcomes: source={} | verified={verified} | unreachable={}",
        outcomes.len(),
        unreachable.len()
    );
    for (name, why) in &unreachable {
        println!("  unreachable: {name} — {why}");
    }
}
