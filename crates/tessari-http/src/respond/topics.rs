//! Per-topic series on `/metrics`, for a scraper that signed in (G042, ADR-0086).
//!
//! # Why only for a scraper that signed in
//!
//! `/metrics` takes no credential, and what it carries without one is
//! operational — an uptime, a sequence, some counts — with no schema in it. A
//! topic's name is schema, and so is a group's: series naming them would tell
//! anybody who can reach the port what a tenant calls its data. So they are
//! answered only to a request that presents a credential, and they are read
//! through that caller's own session, which means the store's grants decide
//! which topics appear: a scraper signed in for one namespace sees that
//! namespace's topics and nobody else's.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use tessaridb::{Db, Outcome, Value};

use super::{Answer, failure, session_for};
use crate::basic::Presented;
use crate::tokens::Tokens;

/// One series of one family: labels and a value.
type Samples = Vec<(String, u64)>;

/// The families, in the order they are written, with their help and type.
const FAMILIES: [(&str, &str, &str); 7] = [
    (
        "tessari_topic_messages",
        "Messages a topic holds now.",
        "gauge",
    ),
    (
        "tessari_topic_last_position",
        "The last position a topic has given.",
        "counter",
    ),
    (
        "tessari_topic_consumer_lag",
        "Messages a reader without a group has not been given.",
        "gauge",
    ),
    (
        "tessari_topic_group_lag",
        "Messages after a group's committed position.",
        "gauge",
    ),
    (
        "tessari_topic_group_in_flight",
        "Messages a group holds unacknowledged.",
        "gauge",
    ),
    (
        "tessari_topic_group_redelivered_total",
        "Deliveries after the first, over a group's life.",
        "counter",
    ),
    (
        "tessari_topic_group_dead_lettered_total",
        "Messages a group gave up on, over its life.",
        "counter",
    ),
];

/// Whether a catalog name may stand in a `USE` statement as it is.
///
/// Names are grammar and never bound, so one is written into a statement only
/// after a check narrower than the lexer's: letters, digits and underscores. A
/// name the catalog holds that fails it is one no statement could have written,
/// and it is skipped rather than quoted.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// A label value in the exposition format's escaping.
fn label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn whole(value: Option<&Value>) -> u64 {
    match value {
        Some(Value::Number(tessaridb::Number::Integer(held))) => u64::try_from(*held).unwrap_or(0),
        _ => 0,
    }
}

fn names(report: &BTreeMap<String, Value>, field: &str) -> Vec<String> {
    match report.get(field) {
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(|name| match name {
                Value::String(name) => Some(name.clone()),
                _ => None,
            })
            .filter(|name| plain(name))
            .collect(),
        _ => Vec::new(),
    }
}

/// The report a script's last statement answered, or nothing.
fn report(session: &mut tessaridb::Session<'_>, script: &str) -> Option<BTreeMap<String, Value>> {
    match session.run(script).ok()?.pop()? {
        Outcome::Value(Value::Object(report)) => Some(report),
        _ => None,
    }
}

/// The per-topic series, or nothing when no credential was presented.
///
/// A credential that is refused is answered as any other route answers it, so
/// a scraper configured with a wrong password is told rather than quietly
/// given fewer series.
pub(crate) fn topic_series(
    db: &Db,
    tokens: &Tokens,
    presented: &Presented,
) -> Result<String, Answer> {
    if matches!(presented, Presented::Nobody) {
        return Ok(String::new());
    }
    let mut session = session_for(db, tokens, presented)?;
    let store = session
        .run("INFO FOR STORE;")
        .map_err(|error| failure(&error))?;
    let Some(Outcome::Value(Value::Object(store))) = store.last() else {
        return Ok(String::new());
    };
    let mut families: BTreeMap<&str, Samples> = BTreeMap::new();
    for namespace in names(store, "namespaces") {
        let Some(held) = report(
            &mut session,
            &format!("USE NAMESPACE {namespace}; INFO FOR NAMESPACE;"),
        ) else {
            continue;
        };
        for database in names(&held, "databases") {
            let Some(held) = report(
                &mut session,
                &format!("USE DATABASE {database}; INFO FOR DATABASE;"),
            ) else {
                continue;
            };
            for topic in names(&held, "topics") {
                let Some(held) = report(&mut session, &format!("INFO FOR TOPIC {topic};")) else {
                    continue;
                };
                let base = format!(
                    "namespace=\"{}\",database=\"{}\",topic=\"{}\"",
                    label(&namespace),
                    label(&database),
                    label(&topic)
                );
                collect(&mut families, &base, &held);
            }
        }
    }
    let mut out = String::new();
    for (family, help, kind) in FAMILIES {
        let Some(samples) = families.get(family) else {
            continue;
        };
        let _ = writeln!(out, "# HELP {family} {help}");
        let _ = writeln!(out, "# TYPE {family} {kind}");
        for (labels, value) in samples {
            let _ = writeln!(out, "{family}{{{labels}}} {value}");
        }
    }
    Ok(out)
}

/// One topic's report, as samples of each family.
fn collect(
    families: &mut BTreeMap<&'static str, Samples>,
    base: &str,
    held: &BTreeMap<String, Value>,
) {
    let last = whole(held.get("last"));
    let messages = match held.get("first") {
        Some(Value::Number(tessaridb::Number::Integer(first))) => {
            u64::try_from(*first).map_or(0, |first| last.saturating_sub(first).saturating_add(1))
        }
        _ => 0,
    };
    let mut push = |family: &'static str, labels: String, value: u64| {
        families.entry(family).or_default().push((labels, value));
    };
    push("tessari_topic_messages", base.to_owned(), messages);
    push("tessari_topic_last_position", base.to_owned(), last);
    if let Some(Value::Object(readers)) = held.get("consumers") {
        for (name, reader) in readers {
            let Value::Object(reader) = reader else {
                continue;
            };
            let labels = format!("{base},consumer=\"{}\"", label(name));
            push(
                "tessari_topic_consumer_lag",
                labels,
                whole(reader.get("lag")),
            );
        }
    }
    if let Some(Value::Object(groups)) = held.get("groups") {
        for (name, group) in groups {
            let Value::Object(group) = group else {
                continue;
            };
            let labels = format!("{base},group=\"{}\"", label(name));
            push(
                "tessari_topic_group_lag",
                labels.clone(),
                whole(group.get("lag")),
            );
            push(
                "tessari_topic_group_in_flight",
                labels.clone(),
                whole(group.get("in_flight")),
            );
            push(
                "tessari_topic_group_redelivered_total",
                labels.clone(),
                whole(group.get("redelivered")),
            );
            push(
                "tessari_topic_group_dead_lettered_total",
                labels,
                whole(group.get("dead_lettered")),
            );
        }
    }
}
