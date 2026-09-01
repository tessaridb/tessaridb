//! The ratchet: a statement form is in the language only if the corpus has a
//! case for it.
//!
//! That sentence is written down in the project's knowledge base as an
//! invariant, and an invariant nothing checks is a sentence. This module makes
//! it a test that fails, in two steps that are hard to get past by accident:
//!
//! 1. [`form_name`] matches **exhaustively**, so adding a statement to the
//!    grammar stops the build here until someone names it.
//! 2. [`FORMS`] is *generated from the same rows as that match*, so naming a
//!    form adds it to the list in the same edit, and the corpus and the
//!    specification are then checked against it until a case and an example
//!    exist.
//!
//! Step 2 used to be a hand-written list beside the match, and a list beside a
//! match is two declarations of one table. Seven forms had already drifted
//! through the gap — they were in the match, in neither the list nor the sample
//! script, and therefore invisible to both tests, so nothing ever demanded a
//! case for them. The rows below are the single declaration that closes it.

use tessari_ql::{Script, StatementKind};

/// The statement forms, declared once.
///
/// Expands to the exhaustive match in [`form_name`] and to [`FORMS`]. A variant
/// missing here fails the match's exhaustiveness check, which is what makes the
/// list impossible to forget.
macro_rules! forms {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        /// What a statement form is called, for coverage.
        #[must_use]
        pub const fn form_name(kind: &StatementKind) -> &'static str {
            match kind {
                $(StatementKind::$variant { .. } => $name,)+
            }
        }

        /// Every form the corpus and the specification must cover.
        pub const FORMS: &[&str] = &[$($name),+];
    };
}

forms! {
    Use => "USE",
    DefineNamespace => "DEFINE NAMESPACE",
    DefineDatabase => "DEFINE DATABASE",
    DefineTable => "DEFINE TABLE",
    DefineSpace => "DEFINE SPACE",
    DefineBucket => "DEFINE BUCKET",
    DefineCollection => "DEFINE COLLECTION",
    DefineVector => "DEFINE VECTOR",
    DropVector => "DROP VECTOR",
    DefineGeo => "DEFINE GEO",
    DropGeo => "DROP GEO",
    DefineGraph => "DEFINE GRAPH",
    DropGraph => "DROP GRAPH",
    DefineEdge => "DEFINE EDGE",
    DropEdge => "DROP EDGE",
    DefineIndex => "DEFINE INDEX",
    DefineField => "DEFINE FIELD",
    DefineAnalyzer => "DEFINE ANALYZER",
    DefineUser => "DEFINE USER",
    AlterUser => "ALTER USER",
    DefineNode => "DEFINE NODE",
    DefineReplica => "DEFINE REPLICA",
    DefineConsumer => "DEFINE CONSUMER",
    DropConsumer => "DROP CONSUMER",
    DropUser => "DROP USER",
    Grant => "GRANT",
    Revoke => "REVOKE",
    GrantAuthority => "GRANT ON REACH",
    RevokeAuthority => "REVOKE ON REACH",
    DropTable => "DROP TABLE",
    DropIndex => "DROP INDEX",
    RebuildIndex => "REBUILD INDEX",
    DropField => "DROP FIELD",
    DropAnalyzer => "DROP ANALYZER",
    DropReplica => "DROP REPLICA",
    DropDatabase => "DROP DATABASE",
    DropNamespace => "DROP NAMESPACE",
    AlterTable => "ALTER TABLE",
    AlterField => "ALTER TABLE ALTER FIELD",
    Relate => "RELATE",
    DeleteEdge => "DELETE EDGE",
    Create => "CREATE",
    Insert => "INSERT",
    Select => "SELECT",
    Update => "UPDATE",
    Upsert => "UPSERT",
    Throw => "THROW",
    Delete => "DELETE",
    DeleteWhere => "DELETE FROM",
    Get => "GET",
    Set => "SET",
    Del => "DEL",
    Put => "PUT",
    Read => "READ",
    Backup => "BACKUP",
    Explain => "EXPLAIN",
    Info => "INFO FOR",
    Keys => "KEYS",
    Let => "LET",
    Return => "RETURN",
    Begin => "BEGIN",
    Commit => "COMMIT",
    Cancel => "CANCEL",
    Verify => "VERIFY",
}

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
    fn the_sample_script_exercises_every_form() {
        // Since `FORMS` is generated from the match, that the two agree is now
        // a compile-time property rather than a test. What still needs proving
        // is that one statement of every form parses to the name it should, so
        // the corpus and specification ratchets have a complete list to demand
        // cases against.
        let script = tessari_ql::parse(
            "USE NAMESPACE n;\
             DEFINE NAMESPACE n;\
             DEFINE DATABASE d;\
             DEFINE TABLE t SCHEMALESS;\
             DEFINE SPACE s;\
             DEFINE BUCKET b;\
             DEFINE COLLECTION c;\
             DEFINE VECTOR v DIMENSION 3 DISTANCE cosine;\
             DROP VECTOR v;\
             DEFINE GEO g;\
             DROP GEO g;\
             DEFINE GRAPH gr;\
             DROP GRAPH gr;\
             DEFINE EDGE e IN gr FROM t TO t;\
             DROP EDGE e;\
             DEFINE INDEX i ON t FIELDS f;\
             DEFINE FIELD f ON t TYPE string;\
             DROP TABLE t;\
             DROP INDEX i ON t;\
             REBUILD INDEX i ON t;\
             DROP FIELD f ON t;\
             DROP ANALYZER a;\
             DROP REPLICA second;\
             DROP DATABASE d;\
             DROP NAMESPACE n;\
             ALTER TABLE t SET SCHEMAFULL;\
             ALTER TABLE t ALTER FIELD f TYPE string;\
             DEFINE ANALYZER a FILTERS lowercase;\
             DEFINE USER u ROLE owner PASSWORD 'x';\
             DEFINE NODE ROLES serving;\
             DEFINE REPLICA second AT 'host:9001';\
             DEFINE CONSUMER c FROM 'b:9092' TOPIC 't' GROUP 'g' FORMAT json \
             INTO t IDENTITY k MAP a AS b ON FAILURE stop;\
             DROP CONSUMER c;\
             DROP USER u;\
             GRANT read ON t TO u;\
             REVOKE read ON t FROM u;\
             GRANT manage ON NAMESPACE n TO u;\
             REVOKE manage ON STORE FROM u;\
             RELATE t:1->e->t:2;\
             DELETE t:1->e->t:2;\
             CREATE t:1 = 1;\
             INSERT INTO t (a) VALUES (1);\
             SELECT * FROM t;\
             UPDATE t:1 = 1;\
             UPSERT t:1 = 1;\
             THROW 'no';\
             DELETE t:1;\
             DELETE FROM t WHERE a = 1 LIMIT ALL;\
             GET s:1;\
             SET s:1 = 1;\
             DEL s:1;\
             KEYS FROM s;\
             PUT b:'/a.txt' = 0x0a;\
             READ b:'/a.txt';\
             BACKUP;\
             EXPLAIN SELECT * FROM t;\
             INFO FOR STORE;\
             INFO FOR NODE;\
             ALTER USER u SET ROLE viewer;\
             LET $x = 1;\
             RETURN $x;\
             BEGIN;\
             COMMIT;\
             CANCEL;\
             VERIFY;",
        )
        .unwrap();
        let mut found = forms_in(&script);
        found.sort_unstable();
        found.dedup();
        // The only assertion left. `found` is built by `form_name`, so it is a
        // subset of `FORMS` by construction; this adds the other direction, and
        // the equality a second assertion used to check now follows from the
        // two. What it means is that the script below exercises every form —
        // that the list and the match agree is no longer a claim a test can
        // fail, because they are one declaration.
        assert!(
            uncovered(&found).is_empty(),
            "the sample script does not exercise every form: {:?}",
            uncovered(&found)
        );
    }
}
