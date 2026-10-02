//! Answering a script: the listing, the run, and each outcome as JSON.

use super::{Answer, name_of, script_failure, session_for};
use crate::basic::Presented;
use crate::json;
use crate::tokens::Tokens;
use std::collections::BTreeMap;
use tessaridb::{Db, Outcome};

/// A bucket's listing: what the files are, and nothing about how they were
/// found.
///
/// This used to hand back the raw statement result wrapped in a key, which
/// published `plan.access`, `plan.table`, `kind` and `path` — planner internals
/// — plus `chunks`, a storage detail (Q-260). A public route's body is a
/// contract in every language a client is written in, so changing the planner
/// would then have broken clients that never asked about it.
///
/// A file listing wants a name, a size and a modification time, and the records
/// already carry exactly those three. A record missing one of them contributes
/// the keys it has rather than a null: the caller asked what is in the bucket,
/// and a key that is absent says the store never recorded it.
pub(crate) fn listing(outcomes: &[Outcome]) -> Answer {
    let records = outcomes
        .last()
        .and_then(Outcome::records)
        .unwrap_or_default();
    let names = json::Names::new();
    let mut body = String::from(r#"{"files":["#);
    for (position, (id, held)) in records.iter().enumerate() {
        if position > 0 {
            body.push(',');
        }
        body.push_str(r#"{"path":"#);
        json::string(&mut body, &id.to_string());
        if let tessaridb::Value::Object(fields) = held {
            for key in ["size", "updated"] {
                if let Some(value) = fields.get(key).filter(|value| value.is_present()) {
                    body.push(',');
                    json::string(&mut body, key);
                    body.push(':');
                    json::write(&mut body, value, &names);
                }
            }
        }
        body.push('}');
    }
    body.push_str("]}");
    Answer::new(200, body)
}

/// `POST /script` — run a script, with the values its parameters bind to.
///
/// Each value arrives **written in TessariQL** and is read by the language, which is
/// what keeps a supplied value from ever being read as grammar (SGA.T2): binding
/// happens after parsing and before the first statement, so `'; DROP TABLE
/// users; --` is a string that says something alarming rather than a statement.
/// A value that would not stand alone in a script is refused here, before
/// anything runs.
pub(crate) fn script(
    db: &Db,
    source: &str,
    written: &BTreeMap<String, String>,
    tokens: &Tokens,
    presented: &Presented,
) -> Answer {
    let mut session = match session_for(db, tokens, presented) {
        Ok(session) => session,
        Err(answer) => return answer,
    };
    let mut given = tessaridb::Parameters::new();
    for (name, value) in written {
        match tessaridb::value_of(value) {
            Ok(held) => {
                given.insert(name.clone(), held);
            }
            Err(reason) => {
                return Answer::bad_request(&format!("parameter {name}: {reason}"));
            }
        }
    }
    // What the caller had selected BEFORE the script ran: a carried request is
    // run again from the start on the node that answers it.
    let selected = (
        session.namespace().map(str::to_owned),
        session.database().map(str::to_owned),
    );
    let ran = session.run_with(source, &given);
    // Coordinated (ADR-0108 D1): carried over the peer link to the node that
    // can answer it, as the caller this request proved, rather than answered
    // with a redirect an HTTP client may not follow.
    if let Err(refused) = &ran
        && !session.landed()
        && tessaridb::travels(source)
        && let Some(coordinator) = db.coordinator()
        && let Some(to) = db.answers_instead(refused)
    {
        return match coordinator.coordinate(&tessaridb::Coordination {
            to,
            user: session.signed_in(),
            namespace: selected.0.as_deref(),
            database: selected.1.as_deref(),
            script: source,
            parameters: &given,
            surface: tessaridb::Surface::Http,
        }) {
            Ok(answer) => Answer {
                status: answer.kind,
                body: answer.body,
                ..Answer::new(200, String::new())
            },
            Err(why) => {
                let mut body = String::from(r#"{"error":"#);
                json::string(&mut body, &why);
                body.push('}');
                Answer::new(502, body)
            }
        };
    }
    match ran {
        Ok(outcomes) => results(db, &outcomes),
        Err(error) => script_failure(db, &error, session.landed(), "/script"),
    }
}

/// A carried request's answer, rendered for an HTTP caller by the node that
/// ran it (ADR-0108 D1).
///
/// A refusal naming yet another node is answered `409` — retry — rather than
/// redirected: the request has made its one hop, and a `Location` would not
/// survive the way back.
#[must_use]
pub fn render_coordinated(
    db: &Db,
    ran: &tessaridb::Result<Vec<Outcome>>,
) -> tessaridb::Coordinated {
    let answer = match ran {
        Ok(outcomes) => results(db, outcomes),
        Err(refused) if db.answers_instead(refused).is_some() => {
            let mut body = String::from(r#"{"error":"#);
            json::string(&mut body, &refused.to_string());
            body.push('}');
            Answer::new(409, body)
        }
        Err(refused) => super::failure(refused),
    };
    tessaridb::Coordinated {
        kind: answer.status,
        body: answer.body,
    }
}

/// Every outcome of a run, as the body `POST /script` answers with.
fn results(db: &Db, outcomes: &[Outcome]) -> Answer {
    // Resolved once for the whole answer rather than per outcome, and only
    // when something in it holds a reference: a record reference carries a
    // table id, and a client receiving `"1:2"` cannot follow it. See
    // `Db::names_in`.
    let referenced: Vec<(tessaridb::RecordId, tessaridb::Value)> = outcomes
        .iter()
        .flat_map(|outcome| match outcome {
            Outcome::Records { records, .. } => records.clone(),
            Outcome::Value(held) => {
                vec![(tessaridb::RecordId::Int(0), held.clone())]
            }
            _ => Vec::new(),
        })
        .collect();
    let names = db.names_in(&referenced).unwrap_or_default();

    let mut body = String::from(r#"{"results":["#);
    for (position, outcome) in outcomes.iter().enumerate() {
        if position > 0 {
            body.push(',');
        }
        encode(&mut body, outcome, &names);
    }
    body.push_str("]}");
    Answer::new(200, body)
}

/// One outcome, as the object a caller parses.
pub(crate) fn encode(body: &mut String, outcome: &Outcome, names: &json::Names) {
    match outcome {
        Outcome::Done => body.push_str(r#"{"kind":"done"}"#),
        Outcome::Value(value) => {
            body.push_str(r#"{"kind":"value""#);
            // A `value` key that is absent means `none`, and one holding `null`
            // means `null`. JSON has one word for both, so the distinction is
            // carried by the key — see `json`.
            if value.is_present() {
                body.push_str(r#","value":"#);
                json::write(body, value, names);
            }
            body.push('}');
        }
        Outcome::Keys(keys) => {
            body.push_str(r#"{"kind":"keys","keys":["#);
            for (position, key) in keys.iter().enumerate() {
                if position > 0 {
                    body.push(',');
                }
                json::string(body, &key.to_string());
            }
            body.push_str("]}");
        }
        Outcome::Records {
            records,
            plan,
            notes,
            suggestion,
            only,
        } => {
            body.push_str(r#"{"kind":"records","path":"#);
            json::string(body, name_of(plan.access));
            // The whole plan beside the one word, because the word alone cannot
            // say which index served the read. `path` stays: it is what every
            // client already reads, the two are rendered from the same field so
            // they cannot disagree, and removing it would break readers for
            // nothing.
            body.push_str(r#","plan":"#);
            json::write(body, &plan.to_value(), names);
            // Written only when there is something to say, so every response
            // that had nothing to report is byte-identical to what it was before
            // notes existed. A reader that wants them handles an absent key,
            // which every JSON reader already does.
            if !notes.is_empty() {
                body.push_str(r#","notes":["#);
                for (position, note) in notes.iter().enumerate() {
                    if position > 0 {
                        body.push(',');
                    }
                    body.push_str(r#"{"kind":"#);
                    json::string(body, note.kind());
                    body.push_str(r#","message":"#);
                    json::string(body, &note.message());
                    body.push('}');
                }
                body.push(']');
            }
            // Three states in two JSON facts, which is what lets this key stay
            // absent from the responses that never asked the question — every
            // read without a `MATCHES` over an indexed field, which is nearly
            // all of them.
            //
            // Absent means no term dictionary was consulted, and that is not a
            // claim about the collection: nothing was looked for. PRESENT AND
            // EMPTY is the claim — a dictionary was asked and holds every term
            // the query named. The two must not collapse, because a client that
            // reads an absent key as "nothing is near" is reporting a negative
            // the server never checked.
            if let Some(suggestion) = suggestion {
                body.push_str(r#","suggestion":{"corrections":["#);
                for (position, correction) in suggestion.corrections().iter().enumerate() {
                    if position > 0 {
                        body.push(',');
                    }
                    body.push_str(r#"{"typed":"#);
                    json::string(body, &correction.typed);
                    body.push_str(r#","instead":"#);
                    json::string(body, &correction.instead);
                    body.push('}');
                }
                body.push_str("]}");
            }
            // Written only when true, for the same reason the notes are written
            // only when there are some: every response from a read that did not
            // say `ONLY` stays byte-identical to what it was before the clause
            // existed. `records` stays an array holding at most one, because
            // changing a key's *type* would break every reader, and the flag is
            // what lets a reader that wants the record take it.
            if *only {
                body.push_str(r#","only":true"#);
            }
            body.push_str(r#","records":["#);
            for (position, (id, record)) in records.iter().enumerate() {
                if position > 0 {
                    body.push(',');
                }
                body.push_str(r#"{"id":"#);
                json::string(body, &id.to_string());
                body.push_str(r#","value":"#);
                json::write(body, record, names);
                body.push('}');
            }
            body.push_str("]}");
        }
        // How many records a conditional delete removed, which is the whole
        // point of a retention statement — `done` would make the operator run a
        // count before and after to learn it.
        Outcome::Removed { count } => {
            body.push_str(r#"{"kind":"removed","count":"#);
            body.push_str(&count.to_string());
            body.push('}');
        }
        // `Outcome` is `#[non_exhaustive]`, so a shape this binary does not know
        // is possible in principle. Answering with its absence is honest;
        // guessing at its content would not be.
        //
        // This arm is correct and it is also where `Removed` hid: it answered
        // `unknown` for a known outcome, and §3.5 defines `unknown` as *a kind
        // this client has never seen*, so a conforming client reported version
        // skew that did not exist. Nothing distinguishes a correct wildcard from
        // one absorbing a known case except enumerating the variants against the
        // arms — which is what `every_outcome_this_build_knows_has_its_own_kind`
        // below does, and why it must gain a case whenever `Outcome` does.
        _ => body.push_str(r#"{"kind":"unknown"}"#),
    }
}
