//! The dynamic catalog rows: `pg_class` and `pg_attribute` hold the store's
//! own user tables, so the port builds them from the store's introspection
//! at answer time — `INFO FOR DATABASE` names the caller-visible tables (the
//! engine already filtered by what this connection may see), `INFO FOR
//! TABLE` describes each one's fields. Nothing here invents: the rows are
//! the catalog's words about itself, re-shaped under the dialect's names.
//!
//! The rows live on one connection's task for the duration of one answer —
//! no state crosses a task boundary, and nothing here is shared mutable.

use std::sync::Arc;

use tessaridb::{Parameters, Value};

use crate::surface::running::{Failed, Held, run_refusal};
use crate::surface::types;
use tessaridb::Outcome;

use super::{Query, Row, Source};

/// The user-table rows one introspection built, shared by every catalog
/// statement in the batch.
#[derive(Debug)]
pub(crate) struct Dynamic {
    pub(crate) namespace: (String, i64),
    pub(crate) classes: Vec<ClassRow>,
    pub(crate) attributes: Vec<AttrRow>,
}

/// One user table: its synthesized identity and name, with the deep
/// describe's derived flags — whether the table holds indexes or events,
/// read from its own report. A view carries its stored read, the text
/// `pg_get_viewdef` hands back character for character.
#[derive(Debug)]
pub(crate) struct ClassRow {
    pub(crate) oid: i64,
    pub(crate) name: String,
    pub(crate) hasindex: bool,
    pub(crate) hastriggers: bool,
    pub(crate) viewdef: Option<String>,
}

/// One column of a user table, as the dialect's clients read
/// `pg_attribute`.
#[derive(Debug)]
pub(crate) struct AttrRow {
    pub(crate) relid: i64,
    pub(crate) name: String,
    pub(crate) type_oid: u32,
    pub(crate) num: i64,
    pub(crate) notnull: bool,
    pub(crate) hasdef: bool,
    /// The stored default's own text, what `pg_get_expr` answers back —
    /// the catalog keeps the expression as written, not a parse of it.
    pub(crate) default: Option<String>,
}

/// Whether any query in the batch names user-table rows.
pub(crate) fn needed(queries: &[&Query]) -> bool {
    queries.iter().any(|query| query.needs_dynamic())
}

/// Whether any query in the batch names attribute rows.
pub(crate) fn wants_attributes(queries: &[&Query]) -> bool {
    queries.iter().any(|query| {
        query
            .relations()
            .iter()
            .any(|name| matches!(*name, "pg_attribute" | "pg_attrdef"))
    })
}

/// Whether any query in the batch asks `pg_get_viewdef` — the one function
/// that needs the stored read, so the per-table report is read for it.
pub(crate) fn wants_viewdefs(queries: &[&Query]) -> bool {
    queries.iter().any(|query| {
        query.projection.iter().any(|item| {
            matches!(
                &item.computed,
                Some(super::super::Computed::Call { path, .. })
                    if path.rsplit('.').next() == Some("pg_get_viewdef")
            )
        })
    })
}

/// Build the user-table rows the batch's queries name, or `None` when no
/// query names them. `pinned` names one table's identity a query pinned —
/// only that table's shape is read; otherwise every visible table's.
pub(crate) async fn load(
    held: &mut Held,
    shared: &Arc<crate::surface::Shared>,
    want_attributes: bool,
    want_viewdefs: bool,
    pinned: Option<i64>,
) -> Result<Option<Dynamic>, crate::surface::errors::Refused> {
    let namespace = held.namespace().to_owned();
    let database = held.database().to_owned();
    let outcomes = held
        .run(shared, "INFO FOR DATABASE;", &Parameters::new())
        .await
        .map_err(failed_refusal)?;
    let Some(Outcome::Value(Value::Object(report))) = outcomes.first() else {
        return Err(crate::surface::errors::Refused::surface(
            crate::surface::errors::code::INTERNAL,
            "the store's database report did not read",
        ));
    };
    let names = name_list(report, "tables");
    let topics: Vec<String> = name_list(report, "topics");
    let vaults: Vec<String> = name_list(report, "vaults");
    let mut classes = names
        .into_iter()
        .filter(|name| !topics.contains(name) && !vaults.contains(name))
        .map(|name| ClassRow {
            oid: synthesize_oid(&format!("{namespace}.{database}.{name}")),
            name,
            // Filled from the table's own report below: the class list is
            // built first, each row's flags after.
            hasindex: false,
            hastriggers: false,
            viewdef: None,
        })
        .collect::<Vec<_>>();
    let mut attributes = Vec::new();
    if want_attributes {
        let wanted: Vec<&ClassRow> = match pinned {
            Some(oid) => classes.iter().filter(|class| class.oid == oid).collect(),
            None => classes.iter().collect(),
        };
        for class in wanted {
            let statement = format!("INFO FOR TABLE {};", class.name);
            let outcomes = held
                .run(shared, &statement, &Parameters::new())
                .await
                .map_err(failed_refusal)?;
            let Some(Outcome::Value(Value::Object(report))) = outcomes.first() else {
                return Err(crate::surface::errors::Refused::surface(
                    crate::surface::errors::code::INTERNAL,
                    "the store's table report did not read",
                ));
            };
            let Some(Value::Array(fields)) = report.get("fields") else {
                continue;
            };
            for (index, field) in fields.iter().enumerate() {
                let Value::Object(described) = field else {
                    continue;
                };
                let Some(Value::String(name)) = described.get("name") else {
                    continue;
                };
                let kind = match described.get("type") {
                    Some(Value::String(kind)) => kind.as_str(),
                    _ => "",
                };
                let default = match described.get("default") {
                    Some(Value::String(text)) => Some(text.clone()),
                    _ => None,
                };
                attributes.push(AttrRow {
                    relid: class.oid,
                    name: name.clone(),
                    type_oid: kind_oid(kind),
                    num: i64::try_from(index.saturating_add(1)).unwrap_or(i64::MAX),
                    notnull: matches!(described.get("required"), Some(Value::Bool(true))),
                    hasdef: default.is_some(),
                    default,
                });
            }
        }
    }
    if want_viewdefs {
        // The stored read, character for character, is what `INFO FOR TABLE`
        // reports under `view` for a view and nothing else for any other
        // kind — the report's own words, no invention.
        for class in &mut classes {
            let outcomes = held
                .run(
                    shared,
                    &format!("INFO FOR TABLE {};", class.name),
                    &Parameters::new(),
                )
                .await
                .map_err(failed_refusal)?;
            if let Some(Outcome::Value(Value::Object(report))) = outcomes.first()
                && let Some(Value::String(read)) = report.get("view")
            {
                class.viewdef = Some(read.to_string());
            }
        }
    }
    Ok(Some(Dynamic {
        namespace: (
            namespace.clone(),
            synthesize_oid(&format!("tessari.pg.namespace.{namespace}")),
        ),
        classes,
        attributes,
    }))
}

/// A string list out of a database report, absent reading as empty.
fn name_list(report: &std::collections::BTreeMap<String, Value>, key: &str) -> Vec<String> {
    match report.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(name) => Some(name.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The wire type a store field kind reads as under a bound parameter — the
/// one map the port infers types through, in both directions.
pub(crate) fn kind_oid(kind: &str) -> u32 {
    match kind {
        "bool" => types::oid::BOOL,
        "int" => types::oid::INT8,
        "float" => types::oid::FLOAT8,
        "decimal" | "number" => types::oid::NUMERIC,
        "string" => types::oid::TEXT,
        "datetime" => types::oid::TIMESTAMPTZ,
        "duration" => types::oid::INTERVAL,
        "uuid" => types::oid::UUID,
        "bytes" => types::oid::BYTEA,
        _ => types::oid::TEXT,
    }
}

/// A deterministic identity for a user table or the connection's namespace,
/// clear of the static catalog's oid range. FNV-1a over the qualified name;
/// a collision with a static oid flips the high bit rather than shadowing a
/// real catalog row.
pub(crate) fn synthesize_oid(qualified: &str) -> i64 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in qualified.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    if STATIC_OIDS.contains(&hash) {
        hash |= 0x8000_0000;
    }
    i64::from(hash)
}

/// The oids the static catalog answers, which a synthesized identity must
/// never shadow.
const STATIC_OIDS: [u32; 22] = [
    11, 13000, 1259, 1247, 1249, 2615, 3541, 16, 17, 20, 21, 23, 25, 26, 700, 701, 705, 1043, 1184,
    1186, 1700, 2950,
];

/// Rows for the relations the dynamic set serves: `pg_class` and
/// `pg_attribute` wholly, and the connection's own namespace row for
/// `pg_namespace`. `None` for everything else — the static set answers.
pub(crate) fn materialize(source: &Source, dynamic: &Dynamic) -> Option<Vec<Row>> {
    match source.name {
        "pg_class" => Some(
            dynamic
                .classes
                .iter()
                .map(|class| {
                    vec![
                        (format!("{}.oid", source.alias), Some(int(class.oid))),
                        (format!("{}.relname", source.alias), Some(text(&class.name))),
                        (
                            format!("{}.relnamespace", source.alias),
                            Some(int(dynamic.namespace.1)),
                        ),
                        (format!("{}.relkind", source.alias), Some(text("r"))),
                        (format!("{}.relam", source.alias), Some(int(0))),
                        (format!("{}.relowner", source.alias), Some(int(10))),
                        (
                            format!("{}.reltuples", source.alias),
                            Some(Value::Number(tessaridb::Number::float(0.0))),
                        ),
                        (format!("{}.relchecks", source.alias), Some(int(0))),
                        (
                            format!("{}.relhasindex", source.alias),
                            Some(Value::Bool(class.hasindex)),
                        ),
                        (
                            format!("{}.relhasrules", source.alias),
                            Some(Value::Bool(false)),
                        ),
                        (
                            format!("{}.relhastriggers", source.alias),
                            Some(Value::Bool(class.hastriggers)),
                        ),
                        (
                            format!("{}.relrowsecurity", source.alias),
                            Some(Value::Bool(false)),
                        ),
                        (
                            format!("{}.relforcerowsecurity", source.alias),
                            Some(Value::Bool(false)),
                        ),
                        (format!("{}.reloftype", source.alias), Some(int(0))),
                        (format!("{}.reltoastrelid", source.alias), Some(int(0))),
                        (
                            format!("{}.relispartition", source.alias),
                            Some(Value::Bool(false)),
                        ),
                        (format!("{}.reltablespace", source.alias), Some(int(0))),
                        (format!("{}.relpersistence", source.alias), Some(text("p"))),
                        (format!("{}.relreplident", source.alias), Some(text("d"))),
                    ]
                })
                .collect(),
        ),
        "pg_attribute" => Some(
            dynamic
                .attributes
                .iter()
                .map(|attr| {
                    vec![
                        (format!("{}.attrelid", source.alias), Some(int(attr.relid))),
                        (format!("{}.attname", source.alias), Some(text(&attr.name))),
                        (
                            format!("{}.atttypid", source.alias),
                            Some(int(i64::from(attr.type_oid))),
                        ),
                        (format!("{}.attnum", source.alias), Some(int(attr.num))),
                        (
                            format!("{}.attnotnull", source.alias),
                            Some(Value::Bool(attr.notnull)),
                        ),
                        (
                            format!("{}.atthasdef", source.alias),
                            Some(Value::Bool(attr.hasdef)),
                        ),
                        (
                            format!("{}.attisdropped", source.alias),
                            Some(Value::Bool(false)),
                        ),
                        (format!("{}.atttypmod", source.alias), Some(int(-1))),
                        (format!("{}.attidentity", source.alias), Some(text(""))),
                        (format!("{}.attgenerated", source.alias), Some(text(""))),
                        (
                            format!("{}.attcollation", source.alias),
                            Some(int(
                                if matches!(attr.type_oid, types::oid::TEXT | types::oid::VARCHAR) {
                                    100
                                } else {
                                    0
                                },
                            )),
                        ),
                    ]
                })
                .collect(),
        ),
        // `pg_attrdef`: one row per column that declares a default, the
        // expression kept as written — `pg_get_expr` answers it back.
        "pg_attrdef" => Some(
            dynamic
                .attributes
                .iter()
                .filter_map(|attr| {
                    let default = attr.default.as_ref()?;
                    Some(vec![
                        (format!("{}.adrelid", source.alias), Some(int(attr.relid))),
                        (format!("{}.adnum", source.alias), Some(int(attr.num))),
                        (format!("{}.adbin", source.alias), Some(text(default))),
                    ])
                })
                .collect(),
        ),
        "pg_namespace" => Some(vec![vec![
            (
                format!("{}.oid", source.alias),
                Some(int(dynamic.namespace.1)),
            ),
            (
                format!("{}.nspname", source.alias),
                Some(text(&dynamic.namespace.0)),
            ),
        ]]),
        _ => None,
    }
}

/// A failed introspection mapped the way the answering paths map it.
fn failed_refusal(failed: Failed) -> crate::surface::errors::Refused {
    run_refusal(failed)
}

fn int(held: i64) -> Value {
    Value::Number(held.into())
}

fn text(held: &str) -> Value {
    Value::String(held.to_owned())
}
