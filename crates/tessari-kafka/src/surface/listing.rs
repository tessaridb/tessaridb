//! Every topic a caller may see, named as a Kafka client names it.
//!
//! The store is walked through the caller's own session — its namespaces, each
//! one's databases, each database's topics — so a level the caller may not
//! read is simply not listed. Topics of the database bare names are read in
//! are listed bare and only bare: listing one topic under two names would let
//! a pattern subscription read it twice.

use std::sync::Arc;

use tessaridb::{Outcome, Parameters, Value};

use crate::surface::Shared;
use crate::surface::names::is_identifier;
use crate::surface::running::{Failed, Held};

/// The identifiers listed under `field` of the last answer, or none.
fn listed(answer: Result<Vec<Outcome>, Failed>, field: &str) -> Vec<String> {
    let Ok(outcomes) = answer else {
        return Vec::new();
    };
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        return Vec::new();
    };
    let Some(Value::Array(names)) = fields.get(field) else {
        return Vec::new();
    };
    names
        .iter()
        .filter_map(|name| match name {
            Value::String(name) if is_identifier(name) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

async fn topics_of(
    shared: &Arc<Shared>,
    held: &mut Held,
    namespace: &str,
    database: &str,
) -> Vec<String> {
    let answer = held
        .run_at(
            shared,
            Some(namespace),
            Some(database),
            "INFO FOR DATABASE;",
            &Parameters::new(),
        )
        .await;
    listed(answer, "topics")
}

/// Every topic the caller may see, fully qualified, and bare in the bare database.
pub(crate) async fn visible(shared: &Arc<Shared>, held: &mut Held) -> Vec<String> {
    let none = Parameters::new();
    let bare = shared.database.clone();
    let mut names = Vec::new();
    let namespaces = listed(
        held.run_at(shared, None, None, "INFO FOR STORE;", &none)
            .await,
        "namespaces",
    );
    for namespace in namespaces {
        let databases = listed(
            held.run_at(shared, Some(&namespace), None, "INFO FOR NAMESPACE;", &none)
                .await,
            "databases",
        );
        for database in databases {
            let is_bare = bare
                .as_ref()
                .is_some_and(|bare| bare.namespace == namespace && bare.database == database);
            if is_bare {
                continue;
            }
            for topic in topics_of(shared, held, &namespace, &database).await {
                names.push(format!("{namespace}.{database}.{topic}"));
            }
        }
    }
    if let Some(bare) = bare {
        names.extend(topics_of(shared, held, &bare.namespace, &bare.database).await);
    }
    names
}
