//! The ratchet: a statement form is in the language only if the corpus has a
//! case for it.
//!
//! That sentence is written down in the project's knowledge base as an
//! invariant, and an invariant nothing checks is a sentence. This module makes
//! it a test that fails, in two steps that are hard to get past by accident:
//!
//! 1. [`form_name`] matches **exhaustively**, so adding a statement to the
//!    grammar stops the build here until someone names it.
//! 2. [`FORMS`] sits directly beneath that match, and the corpus is checked
//!    against it, so the newly named form fails the coverage test until a case
//!    exists.
//!
//! The one manual step is step 2, and it is deliberately adjacent to step 1
//! rather than in another file where it would be forgotten.

use bgv_db_ql::{Script, StatementKind};

/// What a statement form is called, for coverage.
#[must_use]
pub const fn form_name(kind: &StatementKind) -> &'static str {
    match kind {
        StatementKind::Use { .. } => "USE",
        StatementKind::DefineNamespace { .. } => "DEFINE NAMESPACE",
        StatementKind::DefineDatabase { .. } => "DEFINE DATABASE",
        StatementKind::DefineTable { .. } => "DEFINE TABLE",
        StatementKind::DefineSpace { .. } => "DEFINE SPACE",
        StatementKind::DefineBucket { .. } => "DEFINE BUCKET",
        StatementKind::DefineIndex { .. } => "DEFINE INDEX",
        StatementKind::DefineField { .. } => "DEFINE FIELD",
        StatementKind::DefineAnalyzer { .. } => "DEFINE ANALYZER",
        StatementKind::DefineUser { .. } => "DEFINE USER",
        StatementKind::DropUser { .. } => "DROP USER",
        StatementKind::Grant { .. } => "GRANT",
        StatementKind::Revoke { .. } => "REVOKE",
        StatementKind::DropTable { .. } => "DROP TABLE",
        StatementKind::DropIndex { .. } => "DROP INDEX",
        StatementKind::RebuildIndex { .. } => "REBUILD INDEX",
        StatementKind::DropField { .. } => "DROP FIELD",
        StatementKind::Relate { .. } => "RELATE",
        StatementKind::Create { .. } => "CREATE",
        StatementKind::Select(_) => "SELECT",
        StatementKind::Update { .. } => "UPDATE",
        StatementKind::Delete { .. } => "DELETE",
        StatementKind::DeleteWhere { .. } => "DELETE FROM",
        StatementKind::Get { .. } => "GET",
        StatementKind::Set { .. } => "SET",
        StatementKind::Del { .. } => "DEL",
        StatementKind::Put { .. } => "PUT",
        StatementKind::Read { .. } => "READ",
        StatementKind::Backup { .. } => "BACKUP",
        StatementKind::Explain(_) => "EXPLAIN",
        StatementKind::Info { .. } => "INFO FOR",
        StatementKind::Keys { .. } => "KEYS",
        StatementKind::Begin => "BEGIN",
        StatementKind::Commit => "COMMIT",
        StatementKind::Cancel => "CANCEL",
    }
}

/// Every form the corpus must cover.
///
/// Kept beside [`form_name`] so that the compiler's complaint and the list to
/// update are the same screen.
pub const FORMS: &[&str] = &[
    "USE",
    "DEFINE NAMESPACE",
    "DEFINE DATABASE",
    "DEFINE TABLE",
    "DEFINE SPACE",
    "DEFINE BUCKET",
    "DEFINE INDEX",
    "DEFINE FIELD",
    "DEFINE ANALYZER",
    "DEFINE USER",
    "DROP USER",
    "GRANT",
    "REVOKE",
    "DROP TABLE",
    "DROP INDEX",
    "REBUILD INDEX",
    "DROP FIELD",
    "RELATE",
    "CREATE",
    "SELECT",
    "UPDATE",
    "DELETE",
    "DELETE FROM",
    "GET",
    "SET",
    "DEL",
    "PUT",
    "READ",
    "BACKUP",
    "EXPLAIN",
    "INFO FOR",
    "KEYS",
    "BEGIN",
    "COMMIT",
    "CANCEL",
];

/// The forms a script uses.
#[must_use]
pub fn forms_in(script: &Script) -> Vec<&'static str> {
    script
        .statements
        .iter()
        .map(|statement| form_name(&statement.kind))
        .collect()
}

/// The forms in [`FORMS`] that `covered` does not contain.
#[must_use]
pub fn uncovered(covered: &[&str]) -> Vec<&'static str> {
    FORMS
        .iter()
        .copied()
        .filter(|form| !covered.contains(form))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_list_and_the_match_agree_on_every_form() {
        // Not provable by the compiler, so it is proved by parsing one statement
        // of each form and asserting the names come back.
        let script = bgv_db_ql::parse(
            "USE NAMESPACE n;\
             DEFINE NAMESPACE n;\
             DEFINE DATABASE d;\
             DEFINE TABLE t;\
             DEFINE SPACE s;\
             DEFINE BUCKET b;\
             DEFINE INDEX i ON t FIELDS f;\
             DEFINE FIELD f ON t TYPE string;\
             DROP TABLE t;\
             DROP INDEX i ON t;\
             REBUILD INDEX i ON t;\
             DROP FIELD f ON t;\
             DEFINE ANALYZER a FILTERS lowercase;\
             DEFINE USER u ROLE owner PASSWORD 'x';\
             DROP USER u;\
             GRANT read ON t TO u;\
             REVOKE read ON t FROM u;\
             RELATE t:1->e->t:2;\
             CREATE t:1 = 1;\
             SELECT * FROM t;\
             UPDATE t:1 = 1;\
             DELETE t:1;\
             DELETE FROM t WHERE a = 1;\
             GET s:1;\
             SET s:1 = 1;\
             DEL s:1;\
             KEYS FROM s;\
             PUT b:'/a.txt' = 0x0a;\
             READ b:'/a.txt';\
             BACKUP;\
             EXPLAIN SELECT * FROM t;\
             INFO FOR STORE;\
             BEGIN;\
             COMMIT;\
             CANCEL;",
        )
        .unwrap();
        let mut found = forms_in(&script);
        found.sort_unstable();
        found.dedup();
        assert!(
            uncovered(&found).is_empty(),
            "FORMS names something the match does not produce: {:?}",
            uncovered(&found)
        );
        assert_eq!(
            found.len(),
            FORMS.len(),
            "the match produces a form FORMS does not list"
        );
    }
}
