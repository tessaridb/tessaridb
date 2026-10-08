//! The abstract syntax, written back out as TessariQL.
//!
//! This is the other half of [`crate::parse`], and it lives beside it for one
//! reason: the failure worth preventing is the two directions **drifting** — a
//! renderer that emits something the parser no longer accepts. In one crate that
//! is a compile-and-test question; in two it becomes a version question, which
//! is the kind nobody notices until a caller's query is refused.
//!
//! # A statement nobody renders is a compile error
//!
//! The match over [`StatementKind`] is exhaustive and carries **no wildcard**.
//! Adding a statement to the grammar therefore fails to compile here until
//! somebody decides how it is written, which is the same guard the classifiers
//! over this enum already use. `SELECT` is the only form this milestone renders,
//! and that is a staging order rather than a permanent shape: every other form
//! names itself in [`crate::Error::Unrenderable`] instead of vanishing into a
//! catch-all.
//!
//! # Values do not appear here
//!
//! A caller's value reaches a script as a parameter and never as text, so the
//! renderer has nothing to escape and no quoting rules to keep in step with the
//! lexer's. That is not an omission this module works around — it is the
//! property that makes a built query safe, and it is asserted by test rather
//! than claimed.

mod read;
use crate::ast::{
    Expr, FieldPath, Ordering, Projected, Script, Statement, StatementKind, TableRef,
};
use crate::error::{Error, Result};
use crate::token::Span;

pub(crate) use read::{write_expr, write_select};

/// A script, written back out as TessariQL.
///
/// The text is a **normal form** rather than a reproduction: clause order is the
/// grammar's, every projection carries its `AS`, and every compound condition is
/// parenthesised. Two trees that say the same thing render identically, which is
/// what lets the text itself be compared.
///
/// # Errors
///
/// [`Error::Unrenderable`] when the script holds a statement this milestone does
/// not write back out. The failure names the statement rather than the token.
pub fn render(script: &Script) -> Result<String> {
    let mut out = String::new();
    for (at, statement) in script.statements.iter().enumerate() {
        if at > 0 {
            out.push('\n');
        }
        write_statement(&mut out, statement)?;
        out.push(';');
    }
    Ok(out)
}

/// A statement's own text, without its terminator.
fn write_statement(out: &mut String, statement: &Statement) -> Result<()> {
    let span = statement.span;
    // Exhaustive and wildcard-free on purpose: see the module documentation.
    // Each unrendered form is listed by name so that the failure says which
    // statement it met, which a merged arm could not.
    match &statement.kind {
        StatementKind::Select(select) => write_select(out, select),
        StatementKind::Use { .. } => Err(unrenderable("USE", span)),
        StatementKind::DefineNamespace { .. } => Err(unrenderable("DEFINE NAMESPACE", span)),
        StatementKind::DefineDatabase { .. } => Err(unrenderable("DEFINE DATABASE", span)),
        StatementKind::DefineTable { .. } => Err(unrenderable("DEFINE TABLE", span)),
        StatementKind::DefineSpace { .. } => Err(unrenderable("DEFINE SPACE", span)),
        StatementKind::DefineTopic { .. } => Err(unrenderable("DEFINE TOPIC", span)),
        StatementKind::ReadTopic { .. } => Err(unrenderable("READ FROM", span)),
        StatementKind::DefineGroup { .. } => Err(unrenderable("DEFINE GROUP", span)),
        StatementKind::DropGroup { .. } => Err(unrenderable("DROP GROUP", span)),
        StatementKind::AlterGroup { .. } => Err(unrenderable("ALTER GROUP", span)),
        StatementKind::AckTopic { .. } => Err(unrenderable("ACK", span)),
        StatementKind::NackTopic { .. } => Err(unrenderable("NACK", span)),
        StatementKind::DefineBucket { .. } => Err(unrenderable("DEFINE BUCKET", span)),
        StatementKind::DefineCollection { .. } => Err(unrenderable("DEFINE COLLECTION", span)),
        StatementKind::DefineVector { .. } => Err(unrenderable("DEFINE VECTOR", span)),
        StatementKind::DefineQueue { .. } => Err(unrenderable("DEFINE QUEUE", span)),
        StatementKind::DropQueue { .. } => Err(unrenderable("DROP QUEUE", span)),
        StatementKind::DefineSeries { .. } => Err(unrenderable("DEFINE SERIES", span)),
        StatementKind::DropSeries { .. } => Err(unrenderable("DROP SERIES", span)),
        StatementKind::DefineRollup { .. } => Err(unrenderable("DEFINE ROLLUP", span)),
        StatementKind::DropRollup { .. } => Err(unrenderable("DROP ROLLUP", span)),
        StatementKind::DefineView { .. } => Err(unrenderable("DEFINE VIEW", span)),
        StatementKind::DropView { .. } => Err(unrenderable("DROP VIEW", span)),
        StatementKind::DefineEvent { .. } => Err(unrenderable("DEFINE EVENT", span)),
        StatementKind::DropEvent { .. } => Err(unrenderable("DROP EVENT", span)),
        StatementKind::DropIfExists(_) => Err(unrenderable("DROP … IF EXISTS", span)),
        StatementKind::DefineParam { .. } => Err(unrenderable("DEFINE PARAM", span)),
        StatementKind::DropParam { .. } => Err(unrenderable("DROP PARAM", span)),
        StatementKind::Claim { .. } | StatementKind::ClaimRecord { .. } => {
            Err(unrenderable("CLAIM", span))
        }
        StatementKind::Release { .. } => Err(unrenderable("RELEASE", span)),
        StatementKind::ReleaseAll { .. } => Err(unrenderable("RELEASE ALL", span)),
        StatementKind::DropVector { .. } => Err(unrenderable("DROP VECTOR", span)),
        StatementKind::DefineGeo { .. } => Err(unrenderable("DEFINE GEO", span)),
        StatementKind::DropGeo { .. } => Err(unrenderable("DROP GEO", span)),
        StatementKind::DefineVault { .. } => Err(unrenderable("DEFINE VAULT", span)),
        StatementKind::DropVault { .. } => Err(unrenderable("DROP VAULT", span)),
        StatementKind::Reveal { .. } => Err(unrenderable("REVEAL", span)),
        StatementKind::AddRecipient { .. } => Err(unrenderable("ADD RECIPIENT", span)),
        StatementKind::RemoveRecipient { .. } => Err(unrenderable("REMOVE RECIPIENT", span)),
        // Unrenderable like its neighbours, and here the consequence is worth
        // saying out loud: rendering this statement would put a passphrase into
        // a string, and a string is a thing that gets logged.
        StatementKind::UnsealVault { .. } => Err(unrenderable("UNSEAL VAULT", span)),
        StatementKind::SealVault { .. } => Err(unrenderable("SEAL VAULT", span)),
        StatementKind::ChangeVaultPassphrase { .. } => {
            Err(unrenderable("CHANGE VAULT PASSPHRASE", span))
        }
        StatementKind::DefineIndex { .. } => Err(unrenderable("DEFINE INDEX", span)),
        StatementKind::DefineField { .. } => Err(unrenderable("DEFINE FIELD", span)),
        StatementKind::DefineAnalyzer { .. } => Err(unrenderable("DEFINE ANALYZER", span)),
        StatementKind::DefineUser { .. } => Err(unrenderable("DEFINE USER", span)),
        StatementKind::AlterUser { .. } => Err(unrenderable("ALTER USER", span)),
        StatementKind::AlterNamespace { .. } => Err(unrenderable("ALTER NAMESPACE", span)),
        StatementKind::DefineNode { .. } => Err(unrenderable("DEFINE NODE", span)),
        StatementKind::DefineFailover { .. } => Err(unrenderable("DEFINE FAILOVER", span)),
        StatementKind::RevokeCertificate { .. } => Err(unrenderable("REVOKE CERTIFICATE", span)),
        StatementKind::CreateJoinToken { .. } => Err(unrenderable("CREATE JOIN TOKEN", span)),
        StatementKind::DefineReplica { .. } => Err(unrenderable("DEFINE REPLICA", span)),
        StatementKind::DefineConsumer { .. } => Err(unrenderable("DEFINE KAFKA CONSUMER", span)),
        StatementKind::DropConsumer { .. } => Err(unrenderable("DROP KAFKA CONSUMER", span)),
        StatementKind::DefineTopicConsumer { .. } => {
            Err(unrenderable("DEFINE TOPIC CONSUMER", span))
        }
        StatementKind::DropTopicConsumer { .. } => Err(unrenderable("DROP TOPIC CONSUMER", span)),
        StatementKind::Explain(_) => Err(unrenderable("EXPLAIN", span)),
        StatementKind::Info { .. } => Err(unrenderable("INFO", span)),
        StatementKind::Backup { .. } => Err(unrenderable("BACKUP", span)),
        StatementKind::Restore { .. } => Err(unrenderable("RESTORE", span)),
        StatementKind::DropUser { .. } => Err(unrenderable("DROP USER", span)),
        StatementKind::DropField { .. } => Err(unrenderable("DROP FIELD", span)),
        StatementKind::DropTable { .. } => Err(unrenderable("DROP TABLE", span)),
        StatementKind::DropIndex { .. } => Err(unrenderable("DROP INDEX", span)),
        StatementKind::DropAnalyzer { .. } => Err(unrenderable("DROP ANALYZER", span)),
        StatementKind::DefineSearch { .. } => Err(unrenderable("DEFINE SEARCH", span)),
        StatementKind::DropSearch { .. } => Err(unrenderable("DROP SEARCH", span)),
        StatementKind::DefineSynonyms { .. } => Err(unrenderable("DEFINE SYNONYMS", span)),
        StatementKind::DropSynonyms { .. } => Err(unrenderable("DROP SYNONYMS", span)),
        StatementKind::DefineStopwords { .. } => Err(unrenderable("DEFINE STOPWORDS", span)),
        StatementKind::DropStopwords { .. } => Err(unrenderable("DROP STOPWORDS", span)),
        StatementKind::DropReplica { .. } => Err(unrenderable("DROP REPLICA", span)),
        StatementKind::AlterReplica { .. } => Err(unrenderable("ALTER REPLICA", span)),
        StatementKind::FinalizeFormat => Err(unrenderable("ALTER STORE FINALIZE FORMAT", span)),
        StatementKind::DropDatabase { .. } => Err(unrenderable("DROP DATABASE", span)),
        StatementKind::DropNamespace { .. } => Err(unrenderable("DROP NAMESPACE", span)),
        StatementKind::DefineGraph { .. } => Err(unrenderable("DEFINE GRAPH", span)),
        StatementKind::DropGraph { .. } => Err(unrenderable("DROP GRAPH", span)),
        StatementKind::DefineEdge { .. } => Err(unrenderable("DEFINE EDGE", span)),
        StatementKind::DropEdge { .. } => Err(unrenderable("DROP EDGE", span)),
        StatementKind::AlterTable { .. } => Err(unrenderable("ALTER TABLE", span)),
        StatementKind::AlterField { .. } => Err(unrenderable("ALTER TABLE ALTER FIELD", span)),
        StatementKind::RebuildIndex { .. } => Err(unrenderable("REBUILD INDEX", span)),
        StatementKind::CheckTable { .. } => Err(unrenderable("CHECK TABLE", span)),
        StatementKind::AnalyzeTable { .. } => Err(unrenderable("ANALYZE TABLE", span)),
        StatementKind::Grant { .. } => Err(unrenderable("GRANT", span)),
        StatementKind::Revoke { .. } => Err(unrenderable("REVOKE", span)),
        StatementKind::GrantAuthority { .. } => Err(unrenderable("GRANT", span)),
        StatementKind::RevokeAuthority { .. } => Err(unrenderable("REVOKE", span)),
        StatementKind::Relate { .. } => Err(unrenderable("RELATE", span)),
        StatementKind::Create { .. } => Err(unrenderable("CREATE", span)),
        StatementKind::Insert { .. } => Err(unrenderable("INSERT", span)),
        StatementKind::Update { .. } => Err(unrenderable("UPDATE", span)),
        StatementKind::Upsert { .. } => Err(unrenderable("UPSERT", span)),
        StatementKind::Throw { .. } => Err(unrenderable("THROW", span)),
        StatementKind::Delete { .. } => Err(unrenderable("DELETE", span)),
        StatementKind::DeleteEdge { .. } => Err(unrenderable("DELETE of an edge", span)),
        StatementKind::DeleteWhere { .. } => Err(unrenderable("DELETE FROM", span)),
        StatementKind::DeleteSpan { .. } => Err(unrenderable("DELETE FROM a span", span)),
        StatementKind::Get { .. } => Err(unrenderable("GET", span)),
        StatementKind::Set { .. } => Err(unrenderable("SET", span)),
        StatementKind::Expire { .. } => Err(unrenderable("EXPIRE", span)),
        StatementKind::Persist { .. } => Err(unrenderable("PERSIST", span)),
        StatementKind::Incr { .. } => Err(unrenderable("INCR", span)),
        StatementKind::Del { .. } => Err(unrenderable("DEL", span)),
        StatementKind::Put { .. } => Err(unrenderable("PUT", span)),
        StatementKind::Read { .. } => Err(unrenderable("READ", span)),
        StatementKind::Keys { .. } => Err(unrenderable("KEYS", span)),
        StatementKind::Let { .. } => Err(unrenderable("LET", span)),
        StatementKind::Return { .. } => Err(unrenderable("RETURN", span)),
        StatementKind::Begin => Err(unrenderable("BEGIN", span)),
        StatementKind::Commit => Err(unrenderable("COMMIT", span)),
        StatementKind::Cancel => Err(unrenderable("CANCEL", span)),
        StatementKind::Verify => Err(unrenderable("VERIFY", span)),
    }
}

/// The failure a form outside this milestone's coverage raises.
const fn unrenderable(statement: &'static str, span: Span) -> Error {
    Error::Unrenderable { statement, span }
}

/// One sort key. `ASC` is left unwritten, because it is the default and the
/// parser accepts it only as a courtesy.
fn write_ordering(out: &mut String, ordering: &Ordering) -> Result<()> {
    write_expr(out, &ordering.key)?;
    if ordering.descending {
        out.push_str(" DESC");
    }
    Ok(())
}

/// One projected value and the name it answers under.
fn write_projected(out: &mut String, projected: &Projected) -> Result<()> {
    write_expr(out, &projected.value)?;
    out.push_str(" AS ");
    out.push_str(&projected.name.text);
    Ok(())
}

/// `users` or `orders.users`.
fn write_table(out: &mut String, table: &TableRef) {
    if let Some(database) = &table.database {
        out.push_str(&database.text);
        out.push('.');
    }
    out.push_str(&table.name.text);
}

/// A route into a record: `name`, `address.city`, `tags[0]`, `tags[*]`.
fn write_path(out: &mut String, path: &FieldPath) {
    out.push_str(&path.path.to_string());
}

/// `(<left> <word> <right>)` — the shape both connectives share.
fn write_joined(out: &mut String, left: &Expr, word: &str, right: &Expr) -> Result<()> {
    out.push('(');
    write_expr(out, left)?;
    out.push(' ');
    out.push_str(word);
    out.push(' ');
    write_expr(out, right)?;
    out.push(')');
    Ok(())
}
