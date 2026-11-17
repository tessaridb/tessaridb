//! The catalog's static relations: what each holds, and the column types
//! the projection answers under.

use tessaridb::Value;

use crate::surface::types;

/// The static type of a catalog column by its output name: the oid the
/// surface reports it under, and the name it answers with.
pub(crate) fn catalog_type_of(output: &str) -> (u32, String) {
    let name = output.rsplit('.').next().unwrap_or(output);
    let oid = match name {
        "oid" | "typelem" | "typbasetype" | "typrelid" | "rngtypid" | "rngsubtype"
        | "typnamespace" | "relnamespace" | "relam" | "relowner" | "attrelid" | "atttypid" => {
            types::oid::OID
        }
        "attnum" => types::oid::INT4,
        "reltuples" => types::oid::FLOAT8,
        "attnotnull" | "atthasdef" | "attisdropped" => types::oid::BOOL,
        _ => types::oid::TEXT,
    };
    (oid, name.to_owned())
}

/// The static type name an oid serves, the reverse of `catalog_type_of`'s
/// identity columns: a `::regtype` cast resolves through it, and an oid it
/// does not hold is not renamed by guess (the caller keeps the number).
pub(crate) fn type_name_of(oid: i64) -> Option<String> {
    use crate::surface::types::oid as surface_oid;
    let name = match u32::try_from(oid).ok()? {
        x if x == surface_oid::BOOL => "bool",
        x if x == surface_oid::BYTEA => "bytea",
        x if x == surface_oid::INT8 => "int8",
        x if x == surface_oid::INT2 => "int2",
        x if x == surface_oid::INT4 => "int4",
        x if x == surface_oid::TEXT => "text",
        x if x == surface_oid::OID => "oid",
        x if x == surface_oid::FLOAT4 => "float4",
        x if x == surface_oid::FLOAT8 => "float8",
        x if x == surface_oid::VARCHAR => "varchar",
        x if x == surface_oid::TIMESTAMPTZ => "timestamptz",
        x if x == surface_oid::INTERVAL => "interval",
        x if x == surface_oid::NUMERIC => "numeric",
        x if x == surface_oid::UUID => "uuid",
        x if x == surface_oid::JSONB => "jsonb",
        _ => return None,
    };
    Some(name.to_owned())
}

/// One relation: its columns and rows, a row column-ordered.
pub(crate) struct Relation {
    pub(crate) columns: &'static [&'static str],
    pub(crate) rows: Vec<Vec<Value>>,
}

/// `pg_namespace`: the catalog schemas, named for the clients that ask.
/// The connection's own namespace arrives with the dynamic rows.
fn pg_namespace() -> Relation {
    Relation {
        columns: &["oid", "nspname"],
        rows: vec![
            vec![int(11), text("pg_catalog")],
            vec![int(13000), text("information_schema")],
        ],
    }
}

/// `pg_type`: exactly the types this build emits, under the dialect's oids.
fn pg_type() -> Relation {
    const ROWS: &[(u32, &str)] = &[
        (16, "bool"),
        (17, "bytea"),
        (20, "int8"),
        (21, "int2"),
        (23, "int4"),
        (25, "text"),
        (26, "oid"),
        (700, "float4"),
        (701, "float8"),
        (705, "unknown"),
        (1043, "varchar"),
        (1184, "timestamptz"),
        (1186, "interval"),
        (1700, "numeric"),
        (2950, "uuid"),
        (3802, "jsonb"),
    ];
    Relation {
        columns: &[
            "oid",
            "typname",
            "typtype",
            "typelem",
            "typbasetype",
            "typnamespace",
            "typrelid",
            "typcollation",
        ],
        rows: ROWS
            .iter()
            .map(|(oid, name)| {
                vec![
                    int(i64::from(*oid)),
                    text(name),
                    text("b"),
                    int(0),
                    int(0),
                    int(11),
                    int(0),
                    // The collatable types carry the default collation,
                    // what the deep describe's collation subquery compares.
                    int(if matches!(*oid, 25 | 1043) { 100 } else { 0 }),
                ]
            })
            .collect(),
    }
}

/// The listing relations this build holds empty: policies, roles, rewrite
/// rules and inheritance — the dialect's `\d` asks after each, and an empty
/// relation answers an empty listing, which is the truth of this build
/// rather than a gap dressed as one.
fn empty(name: &str) -> Relation {
    let columns: &[&str] = match name {
        "pg_policy" => &[
            "oid",
            "polname",
            "polpermissive",
            "polroles",
            "polrelid",
            "polqual",
            "polwithcheck",
            "polcmd",
        ],
        "pg_roles" => &["oid", "rolname"],
        "pg_rewrite" => &["oid", "rulename", "ev_class"],
        "pg_statistic_ext" => &[
            "oid",
            "stxrelid",
            "stxnamespace",
            "stxname",
            "stxkeys",
            "stxkind",
            "stxstattarget",
        ],
        "pg_publication" => &["oid", "pubname", "puballtables"],
        "pg_publication_rel" => &["oid", "prpubid", "prrelid"],
        _ => &["oid", "inhrelid", "inhparent", "inhseqno"],
    };
    Relation {
        columns,
        rows: Vec::new(),
    }
}

/// `pg_collation`: the one collation this build's text columns carry —
/// the dialect's default, under its own well-known identity.
fn pg_collation() -> Relation {
    Relation {
        columns: &["oid", "collname"],
        rows: vec![vec![int(100), text("default")]],
    }
}

/// `pg_range`: this build emits no range types; the LEFT JOIN answers NULLs.
fn pg_range() -> Relation {
    Relation {
        columns: &["rngtypid", "rngsubtype"],
        rows: Vec::new(),
    }
}

/// `pg_am`: the store serves tables through one access path and names no
/// access-method catalog, so the relation answers no rows — a join against
/// it (the dialect's clients always left-join it) finds nothing, exactly.
fn pg_am() -> Relation {
    Relation {
        columns: &["oid", "amname"],
        rows: Vec::new(),
    }
}

/// The static relations by name. `pg_class` and `pg_attribute` live only in
/// the dynamic rows the store's introspection builds.
fn relations() -> Vec<(&'static str, Relation)> {
    vec![
        ("pg_type", pg_type()),
        ("pg_namespace", pg_namespace()),
        ("pg_range", pg_range()),
        ("pg_am", pg_am()),
        ("pg_collation", pg_collation()),
        ("pg_policy", empty("pg_policy")),
        ("pg_roles", empty("pg_roles")),
        ("pg_rewrite", empty("pg_rewrite")),
        ("pg_inherits", empty("pg_inherits")),
        ("pg_statistic_ext", empty("pg_statistic_ext")),
        ("pg_publication", empty("pg_publication")),
        ("pg_publication_rel", empty("pg_publication_rel")),
    ]
}

pub(crate) fn relation(name: &str) -> Option<Relation> {
    relations()
        .into_iter()
        .find(|(held, _)| *held == name)
        .map(|(_, rel)| rel)
}

fn int(held: i64) -> Value {
    Value::Number(held.into())
}

fn text(held: &str) -> Value {
    Value::String(held.to_owned())
}
