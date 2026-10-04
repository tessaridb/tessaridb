//! `RESTORE SCRIPT FROM '<name>'`: a script backup run into a live store.
//!
//! A script is the one backup written in names rather than ids, so it is the
//! one that can land beside what a store already holds. What makes that safe is
//! that a restore only ever **creates**: before anything runs, the script is
//! read statement by statement against the store, and it may define databases
//! that do not exist yet, the namespaces around them (one that exists is reused
//! rather than defined again), the analyzers their fields use, and the tables,
//! fields, indexes and records inside what it created. A database that already
//! exists refuses the whole restore, and so does any statement outside that —
//! a delete, a drop, a user, a grant, a write into a place it did not create.
//! Then it runs as the caller, with each statement checked again as that
//! caller's own, in two transactions. The first creates the new namespaces and
//! databases, empty: a `USE` is checked against the committed catalog — that is
//! what keeps a signed-in caller from learning which names exist — so a place
//! must exist before the statements that fill it can select it. The second
//! fills them, the script's own `BEGIN … COMMIT` batches folding into it; if it
//! fails, the places the first created are dropped again, so a refused restore
//! leaves nothing behind.

use std::collections::BTreeSet;
use std::fs;

use tessari_ql::{Parameters, Script, StatementKind};
use tessari_storage::Catalog;
use tessari_types::{Number, Value};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// Read `name` from the backup folder, vet it, and run it: its new
    /// namespaces, then its new databases, then what fills them.
    pub(crate) fn restore(&self, name: &str) -> Result<Outcome> {
        let folder = self.backups.as_deref().ok_or(Error::NoBackupFolder)?;
        let path = crate::backup_to::readable(folder, name)?;
        // A sealed script opens with this node's key and is read whole before
        // any of it is vetted, so a file cut or altered refuses here.
        let mut text = String::new();
        fs::File::open(&path)
            .and_then(|file| tessari_vault::at_rest::reading(self.at_rest.as_deref(), file))
            .and_then(|mut opened| std::io::Read::read_to_string(&mut opened, &mut text))
            .map_err(|failure| Error::BackupFailed {
                reason: format!("{}: {failure}", path.display()),
            })?;
        let script = tessari_ql::parse(&text)
            .and_then(|script| script.bind(&Parameters::new()))
            .map_err(|failure| {
                refused(&format!(
                    "{name} is not a script this node reads: {failure}"
                ))
            })?;
        let (script, created) = vetted(self, script, &text)?;
        let statements = script.statements.len();
        let (places, body): (Vec<_>, Vec<_>) =
            script.statements.into_iter().partition(|statement| {
                matches!(
                    statement.kind,
                    StatementKind::DefineNamespace { .. } | StatementKind::DefineDatabase { .. }
                )
            });
        // Namespaces, then databases, then what fills them: each step selects
        // what the one before committed. The skeleton is written from the names
        // rather than taken from the script, because a database needs its
        // namespace selected first.
        let mut namespaces = String::new();
        let mut defined = BTreeSet::new();
        for statement in &places {
            if let StatementKind::DefineNamespace { name, .. } = &statement.kind {
                defined.insert(name.text.clone());
                namespaces.push_str(&format!("DEFINE NAMESPACE {};\n", name.text));
            }
        }
        let mut databases = String::new();
        for place in &created {
            if let Some((within, database)) = place.split_once('.') {
                databases.push_str(&format!(
                    "USE NAMESPACE {within}; DEFINE DATABASE {database};\n"
                ));
            }
        }

        let mut restorer = Session::new(self.store);
        restorer.identity = self.identity.clone();
        restorer.atomically(|held| held.run_with(&namespaces, &Parameters::new()))?;
        if let Err(failure) =
            restorer.atomically(|held| held.run_with(&databases, &Parameters::new()))
        {
            undone(&mut restorer, &[], &defined);
            return Err(failure);
        }
        let filled = restorer.atomically(|held| {
            held.run_parsed(Script {
                statements: body,
                span: script.span,
            })
        });
        if let Err(failure) = filled {
            undone(&mut restorer, &created, &defined);
            return Err(failure);
        }

        let mut answer = std::collections::BTreeMap::new();
        answer.insert("path".to_owned(), Value::String(path.display().to_string()));
        let statements = i64::try_from(statements).map_err(|_| Error::BackupFailed {
            reason: "the script is longer than a count can say".to_owned(),
        })?;
        answer.insert(
            "statements".to_owned(),
            Value::Number(Number::Integer(statements)),
        );
        answer.insert(
            "databases".to_owned(),
            Value::Array(created.into_iter().map(Value::String).collect()),
        );
        Ok(Outcome::Value(Value::Object(answer)))
    }
}

/// The statements that take away what a failed restore created: its
/// databases, then the namespaces it defined.
fn undo(created: &[String], defined: &BTreeSet<String>) -> String {
    let mut undone = String::new();
    for place in created {
        if let Some((within, database)) = place.split_once('.') {
            undone.push_str(&format!(
                "USE NAMESPACE {within}; DROP DATABASE {database};\n"
            ));
        }
    }
    for namespace in defined {
        undone.push_str(&format!("DROP NAMESPACE {namespace};\n"));
    }
    undone
}

/// Take away what a failed restore created, and say so on the node's log when
/// that is refused too — the caller is answered with the refusal that stopped
/// the restore, and the operator is the one who can remove what is left.
fn undone(restorer: &mut Session<'_>, created: &[String], defined: &BTreeSet<String>) {
    if let Err(failure) = restorer.run(&undo(created, defined)) {
        tracing::warn!(
            created = %created.join(", "),
            defined = %defined.iter().cloned().collect::<Vec<_>>().join(", "),
            error = %failure,
            "a refused restore could not take away what it created"
        );
    }
}

fn refused(reason: &str) -> Error {
    Error::RestoreRefused {
        reason: reason.to_owned(),
    }
}

/// The first two words of a statement, which name it without repeating what it
/// carries — a `DEFINE USER` line holds a credential.
fn named(text: &str, statement: &tessari_ql::Statement) -> String {
    text.get(statement.span.start..statement.span.end)
        .unwrap_or_default()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a statement belongs inside a database the restore created.
const fn placed(kind: &StatementKind) -> bool {
    matches!(
        kind,
        StatementKind::DefineTable { .. }
            | StatementKind::DefineGraph { .. }
            | StatementKind::DefineEdge { .. }
            | StatementKind::DefineSpace { .. }
            | StatementKind::DefineTopic { .. }
            | StatementKind::DefineBucket { .. }
            | StatementKind::DefineCollection { .. }
            | StatementKind::DefineVector { .. }
            | StatementKind::DefineGeo { .. }
            | StatementKind::DefineVault { .. }
            | StatementKind::DefineIndex { .. }
            | StatementKind::DefineSearch { .. }
            | StatementKind::DefineField { .. }
            | StatementKind::DefineQueue { .. }
            | StatementKind::DefineSeries { .. }
            | StatementKind::DefineView { .. }
            | StatementKind::Create { .. }
            | StatementKind::Insert { .. }
            | StatementKind::Upsert { .. }
            | StatementKind::Set { .. }
            | StatementKind::Put { .. }
            | StatementKind::Relate { .. }
    )
}

/// The script with the definitions of namespaces that already exist taken out,
/// and the databases it creates — or the refusal that says why it may not run.
fn vetted(session: &Session<'_>, script: Script, text: &str) -> Result<(Script, Vec<String>)> {
    let mut view = session.store.begin()?;
    let catalog = Catalog::new(&mut view);
    let mut namespaces = BTreeSet::new();
    let mut created: Vec<(String, String)> = Vec::new();
    let mut namespace: Option<String> = None;
    let mut database: Option<String> = None;
    let mut kept = Vec::with_capacity(script.statements.len());
    for statement in script.statements {
        match &statement.kind {
            StatementKind::DefineNamespace { name, .. } => {
                namespaces.insert(name.text.clone());
                if catalog.namespace_id(&name.text)?.is_some() {
                    continue;
                }
            }
            StatementKind::DefineDatabase { name, .. } => {
                let Some(within) = namespace.as_deref() else {
                    return Err(refused(&format!(
                        "`{}` defines a database before choosing a namespace",
                        named(text, &statement)
                    )));
                };
                let exists = match catalog.namespace_id(within)? {
                    Some(id) => catalog.database_id(id, &name.text)?.is_some(),
                    None => false,
                };
                if exists {
                    return Err(Error::RestoreTargetExists {
                        place: format!("{within}.{}", name.text),
                    });
                }
                created.push((within.to_owned(), name.text.clone()));
            }
            StatementKind::Use {
                namespace: chosen_namespace,
                database: chosen_database,
                consumer: None,
            } => {
                if let Some(chosen) = chosen_namespace {
                    if !namespaces.contains(&chosen.text) {
                        return Err(refused(&format!(
                            "it uses namespace {} without defining it, which would write into \
                             a place the restore did not create",
                            chosen.text
                        )));
                    }
                    namespace = Some(chosen.text.clone());
                    database = None;
                }
                if let Some(chosen) = chosen_database {
                    let within = namespace.clone().unwrap_or_default();
                    if !created.contains(&(within.clone(), chosen.text.clone())) {
                        return Err(refused(&format!(
                            "it uses database {within}.{} without creating it, which would \
                             write into a place the restore did not create",
                            chosen.text
                        )));
                    }
                    database = Some(chosen.text.clone());
                }
            }
            // The script batches its records between these; the restore is
            // one transaction already, so the batches fold into it.
            StatementKind::Begin | StatementKind::Commit => continue,
            StatementKind::DefineAnalyzer { .. }
            | StatementKind::DefineSynonyms { .. }
            | StatementKind::DefineStopwords { .. } => {}
            kind if placed(kind) => {
                if database.is_none() {
                    return Err(refused(&format!(
                        "`{}` comes before the script chose a database it created",
                        named(text, &statement)
                    )));
                }
            }
            _ => {
                return Err(refused(&format!(
                    "`{}` is not something a restore does — it only creates databases and fills \
                     them; a whole-store script restores into an empty store with -f",
                    named(text, &statement)
                )));
            }
        }
        kept.push(statement);
    }
    view.rollback();
    let places = created
        .into_iter()
        .map(|(within, name)| format!("{within}.{name}"))
        .collect();
    Ok((
        Script {
            statements: kept,
            ..script
        },
        places,
    ))
}
