//! Analyzers, users and grants — the catalog a script writes outside a table —
//! and the parts of the catalog it does not write at all.

use std::fmt::Write as _;

use tessari_storage::{Catalog, Reach, UserDefinition};
use tessari_types::TableId;

use crate::error::Result;
use crate::info::{named_database, named_namespace};
use crate::session::Session;

/// Every analyzer, once: an analyzer is the store's, not a database's.
pub(super) fn analyzers(catalog: &Catalog<'_, '_>) -> Result<String> {
    let mut written = String::new();
    for analyzer in catalog.analyzers()? {
        let filters: Vec<&str> = analyzer
            .analyzer
            .filters()
            .iter()
            .map(|filter| filter.name())
            .collect();
        let _ = writeln!(
            written,
            "DEFINE ANALYZER {} FILTERS {};",
            analyzer.name,
            filters.join(", ")
        );
    }
    Ok(written)
}

/// The catalog kinds a script has no statement for, each named once.
///
/// Graph memberships and declared edge endpoints are refused per table by the
/// declaration writer; what is left here is what belongs to no table.
pub(super) fn uncarried(catalog: &Catalog<'_, '_>) -> Result<Vec<String>> {
    let mut refused = Vec::new();
    for namespace in catalog.namespaces()? {
        for database in catalog.databases_in(namespace.id)? {
            let place = format!("{}.{}", namespace.name, database.name);
            for graph in catalog.graphs_in(namespace.id, database.id)? {
                refused.push(format!("{place}: graph `{}`", graph.name));
            }
            for kind in catalog.edge_kinds_in(namespace.id, database.id)? {
                refused.push(format!("{place}: edge kind `{}`", kind.name));
            }
        }
    }
    let replicas = catalog.replicas()?.len();
    if replicas > 0 {
        refused.push(format!(
            "cluster topology: {replicas} replica declaration(s)"
        ));
    }
    if catalog.failover()?.is_some() {
        refused.push("cluster topology: the failover policy".to_owned());
    }
    let consumers = catalog.consumers()?.len();
    if consumers > 0 {
        refused.push(format!("{consumers} consumer declaration(s)"));
    }
    Ok(refused)
}

/// Every user and every grant, in one transaction at the end of the script.
///
/// At the end because the first user closes the store, and in one transaction
/// so the grants land with the users they name rather than after a close that
/// would refuse them.
pub(super) fn users(
    reader: &Session<'_>,
    placed: &[(TableId, String, String, String)],
) -> Result<String> {
    let mut view = reader.store.begin()?;
    let catalog = Catalog::new(&mut view);
    let users = catalog.users()?;
    if users.is_empty() {
        return Ok(String::new());
    }
    let mut written = String::from("BEGIN;\n");
    for user in &users {
        declare(&catalog, user, &mut written)?;
    }
    for user in &users {
        for grant in catalog.grants_for(user.id)? {
            let Some((_, namespace, database, table)) =
                placed.iter().find(|(id, ..)| *id == grant.table)
            else {
                continue;
            };
            let verbs: Vec<&str> = grant.verbs.iter().map(|verb| verb.name()).collect();
            let fields = if grant.fields.is_empty() {
                String::new()
            } else {
                format!(" FIELDS {}", grant.fields.join(", "))
            };
            let _ = writeln!(
                written,
                "USE NAMESPACE {namespace}; USE DATABASE {database}; GRANT {} ON {table}{fields} TO {};",
                verbs.join(", "),
                user.name
            );
        }
    }
    written.push_str("COMMIT;\n");
    Ok(written)
}

/// `DEFINE USER` with the authorities held at the user's own reach, and a
/// `GRANT` for each held anywhere else.
fn declare(catalog: &Catalog<'_, '_>, user: &UserDefinition, written: &mut String) -> Result<()> {
    let own = match (user.namespace, user.database) {
        (None, _) => Reach::Store,
        (Some(namespace), None) => Reach::Namespace(namespace),
        (Some(namespace), Some(database)) => Reach::Database(namespace, database),
    };
    let scope = match (user.namespace, user.database) {
        (None, _) => String::new(),
        (Some(namespace), database) => format!(" ON {}", reach(catalog, namespace, database)?),
    };
    let at_own: Vec<&str> = user
        .authorities
        .iter()
        .filter(|held| held.reach == own)
        .map(|held| held.kind.name())
        .collect();
    let _ = writeln!(
        written,
        "DEFINE USER {}{scope} AUTHORITIES {} PASSHASH '{}';",
        user.name,
        at_own.join(", "),
        user.secret
    );
    for held in user.authorities.iter().filter(|held| held.reach != own) {
        let target = match held.reach {
            Reach::Store => "STORE".to_owned(),
            Reach::Namespace(namespace) => {
                format!("NAMESPACE {}", named_namespace(catalog, namespace)?)
            }
            Reach::Database(namespace, database) => {
                format!("DATABASE {}", reach(catalog, namespace, Some(database))?)
            }
            Reach::Shard(..) => continue,
        };
        let _ = writeln!(
            written,
            "GRANT {} ON {target} TO {};",
            held.kind.name(),
            user.name
        );
    }
    Ok(())
}

/// A namespace, or a namespace and database, by name.
fn reach(
    catalog: &Catalog<'_, '_>,
    namespace: tessari_types::NamespaceId,
    database: Option<tessari_types::DatabaseId>,
) -> Result<String> {
    let named = named_namespace(catalog, namespace)?;
    Ok(match database {
        Some(database) => format!("{named}.{}", named_database(catalog, database)?),
        None => named,
    })
}
