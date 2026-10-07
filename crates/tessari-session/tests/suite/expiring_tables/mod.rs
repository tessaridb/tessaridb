//! Tables and collections whose records expire (G069 SG2, ADR-0122 Part A).

mod declaration;
mod reads;
mod unique;
mod writes;

use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

/// A session in `prod`/`app`.
fn opened(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE app; USE DATABASE app;",
        )
        .unwrap();
    session
}

/// The last statement's outcome.
fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session.run(script).unwrap().pop().unwrap()
}

/// What an `INFO FOR …` answers.
fn info(session: &mut Session<'_>, script: &str) -> Value {
    match run(session, script) {
        Outcome::Value(value) => value,
        other => panic!("{script} answered {other:?}"),
    }
}

/// One field of an object, `NONE` when it is absent or the value is not one.
fn field(value: &Value, name: &str) -> Value {
    match value {
        Value::Object(fields) => fields.get(name).cloned().unwrap_or(Value::None),
        _ => Value::None,
    }
}
