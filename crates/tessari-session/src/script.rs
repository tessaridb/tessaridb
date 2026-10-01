//! A store's current state, written as TessariQL that rebuilds it (ADR-0091).
//!
//! # The shape
//!
//! One file, in the order a restore needs: a header naming the build, the
//! version read and **everything the script does not carry**; the analyzers;
//! then per namespace and database the declarations, the data and — after the
//! data, as a dump's post-data section does — the indexes; and last, in one
//! transaction, the users and their grants. Last because the first user closes
//! the store, so anything after it would need somebody signed in.
//!
//! It restores with `tessaridb <empty store> -f`, which stops at the first
//! refusal and keeps what came before it — a script that does not restore says
//! so where it stopped.
//!
//! # Nothing is dropped in silence
//!
//! A part with no faithful spelling is refused by name in the header rather than
//! written approximately; [`describe`](crate::describe) already holds that rule
//! for declarations, and this module holds it for everything around them. Kinds
//! the language has no statement for at all — a vault's secrets, a cluster's
//! topology, a reader's position in a topic — are listed the same way, with the
//! snapshot named as the format that carries them.
//!
//! # Read as the language reads
//!
//! The data is read by statements on a session of its own — `SELECT`, `READ`,
//! `RETURN TTL` — rather than off the keyspace, so what the script writes is
//! what a reader of the store is answered, and it never moves the caller's own
//! `USE`.

mod access;
mod data;

use std::fmt::Write as _;

use tessari_storage::{Catalog, Reach, Store, TableDefinition, TableKind};
use tessari_types::{DatabaseId, NamespaceId, TableId};

use crate::error::Result;
use crate::session::Session;

/// What a script holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptTaken {
    /// The script itself.
    pub text: String,
    /// How many records it writes.
    pub records: u64,
    /// Every part it does not carry, as the header names it.
    pub refused: Vec<String>,
}

/// Write the state of `store` as a script, for a process that holds the store
/// and no session — the command line's `--dump`.
///
/// Read as a store-wide owner when the store has users, since the process
/// already holds every byte of it; as nobody when it has none. A session asking
/// with `BACKUP SCRIPT` goes through [`Session::state_script`] instead, as
/// itself.
///
/// # Errors
///
/// Returns an error when the store cannot be read.
pub fn write_script(store: &Store) -> Result<ScriptTaken> {
    let owner = {
        let mut view = store.begin()?;
        Catalog::new(&mut view)
            .users()?
            .into_iter()
            .find(|user| {
                user.authorities.iter().any(|held| {
                    held.kind == tessari_storage::Kind::Operate
                        && held.reach == tessari_storage::Reach::Store
                })
            })
            .map(|user| user.id)
    };
    let mut reader = Session::new(store);
    if let Some(id) = owner {
        reader.acting_as(id)?;
    }
    written(&mut reader, &[])
}

impl Session<'_> {
    /// This session's store as a script — or the part `of` names — read with
    /// this session's identity on a session of its own, so the caller's `USE`
    /// is left where it was.
    pub(crate) fn state_script(&self, of: &[tessari_ql::ReachRef]) -> Result<ScriptTaken> {
        let part = {
            let mut view = self.store.begin()?;
            of.iter()
                .map(|named| self.reach_of(&mut view, named))
                .collect::<Result<Vec<Reach>>>()?
        };
        let mut reader = Session::new(self.store);
        reader.identity = self.identity.clone();
        written(&mut reader, &part)
    }
}

/// One namespace and the tables of each of its databases.
struct Placed {
    namespace: String,
    databases: Vec<(String, Vec<TableDefinition>, String)>,
}

/// Whether a database is carried: every one when `part` is empty, else those
/// it names and those of a namespace it names.
fn carried(part: &[Reach], namespace: NamespaceId, database: DatabaseId) -> bool {
    part.is_empty()
        || part.contains(&Reach::Namespace(namespace))
        || part.contains(&Reach::Database(namespace, database))
}

fn written(reader: &mut Session<'_>, part: &[Reach]) -> Result<ScriptTaken> {
    let mut body = String::new();
    let mut refused = Vec::new();
    let mut records = 0_u64;

    let (placed, names, analyzers, catalog_refusals) = {
        let mut view = reader.store.begin()?;
        let catalog = Catalog::new(&mut view);
        let mut placed = Vec::new();
        let mut names = tessari_ql::literal::Names::new();
        for namespace in catalog.namespaces()? {
            let whole = part.contains(&Reach::Namespace(namespace.id));
            let mut databases = Vec::new();
            for database in catalog.databases_in(namespace.id)? {
                if !carried(part, namespace.id, database.id) {
                    continue;
                }
                // A bucket's chunks and an edge kind's edges live in tables no
                // statement can name; the chunks travel as the file `PUT`
                // writes, and an edge kind is named among what is not carried.
                let tables: Vec<_> = catalog
                    .tables_in(namespace.id, database.id)?
                    .into_iter()
                    .filter(|table| crate::info::nameable(&table.name))
                    .collect();
                for table in &tables {
                    names.insert(table.id, table.name.clone());
                }
                let searches = crate::engine::searches_script(&catalog, namespace.id, database.id)?;
                databases.push((database.name, tables, searches));
            }
            if part.is_empty() || whole || !databases.is_empty() {
                placed.push(Placed {
                    namespace: namespace.name,
                    databases,
                });
            }
        }
        (
            placed,
            names,
            access::analyzers(&catalog, part)?
                + &crate::engine::word_sets_script(&catalog, !part.is_empty())?,
            access::uncarried(&catalog, part)?,
        )
    };
    refused.extend(catalog_refusals);
    body.push_str(&analyzers);

    let mut indexes = String::new();
    for namespace in &placed {
        let _ = writeln!(
            body,
            "DEFINE NAMESPACE {0}; USE NAMESPACE {0};",
            namespace.namespace
        );
        for (database, tables, searches) in &namespace.databases {
            let place = format!("{}.{database}", namespace.namespace);
            let selection = format!(
                "USE NAMESPACE {}; USE DATABASE {database};\n",
                namespace.namespace
            );
            let _ = writeln!(body, "DEFINE DATABASE {database}; USE DATABASE {database};");
            reader.run(&selection)?;
            let mut writable = Vec::new();
            for table in tables {
                match data::declared(reader, table)? {
                    Ok((declaration, table_indexes)) => {
                        body.push_str(&declaration);
                        if !table_indexes.is_empty() {
                            let _ = write!(indexes, "{selection}{table_indexes}");
                        }
                        writable.push(table);
                    }
                    Err(part) => refused.push(format!("{place}.{}: {part}", table.name)),
                }
            }
            for table in writable {
                if let Some(note) = data::note(table) {
                    refused.push(format!("{place}.{}: {note}", table.name));
                }
                if holds_records(&table.kind) {
                    let written = data::records(reader, table, &names, &mut body)?;
                    records = records.saturating_add(written);
                }
            }
            if !searches.is_empty() {
                let _ = write!(indexes, "{selection}{searches}");
            }
        }
    }
    body.push_str(&indexes);
    let mut text = String::from("-- TessariDB state script (ADR-0091)\n");
    if part.is_empty() {
        body.push_str(&access::users(reader, &placed_ids(reader.store)?)?);
        let _ = writeln!(
            text,
            "-- written by {} — restore into an EMPTY store with `tessaridb <store> -f <this file>`",
            tessari_storage::BUILD_VERSION
        );
    } else {
        // Users are the store's, not a namespace's, so a part carries none; and
        // an analyzer is the store's too, so the ones its fields use are declared
        // only where the restoring store has none of that name.
        refused.push("users and grants — a part of the store carries none".to_owned());
        let places: Vec<String> = placed
            .iter()
            .flat_map(|namespace| {
                namespace
                    .databases
                    .iter()
                    .map(move |(database, ..)| format!("{}.{database}", namespace.namespace))
            })
            .collect();
        let _ = writeln!(
            text,
            "-- written by {} — a PART of the store: {}",
            tessari_storage::BUILD_VERSION,
            places.join(", ")
        );
        text.push_str(
            "-- restore where none of these namespaces and databases exists; an analyzer of the \
             same name already there is kept as it is\n",
        );
    }
    if refused.is_empty() {
        text.push_str("-- carries every part of the store it was taken from\n");
    } else {
        text.push_str("-- NOT carried (a state snapshot, `--snapshot`, carries these):\n");
        for part in &refused {
            let _ = writeln!(text, "--   {part}");
        }
    }
    text.push_str(&body);
    Ok(ScriptTaken {
        text,
        records,
        refused,
    })
}

/// Where each table lives, for the grants that name tables by id.
fn placed_ids(store: &Store) -> Result<Vec<(TableId, String, String, String)>> {
    let mut view = store.begin()?;
    let catalog = Catalog::new(&mut view);
    let mut placed = Vec::new();
    for namespace in catalog.namespaces()? {
        for database in catalog.databases_in(namespace.id)? {
            for table in catalog.tables_in(namespace.id, database.id)? {
                placed.push((
                    table.id,
                    namespace.name.clone(),
                    database.name.clone(),
                    table.name,
                ));
            }
        }
    }
    Ok(placed)
}

/// Whether a table's kind carries records a script writes.
const fn holds_records(kind: &TableKind) -> bool {
    !matches!(kind, TableKind::View(_) | TableKind::Vault(_))
}
