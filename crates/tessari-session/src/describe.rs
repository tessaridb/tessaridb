//! A table's declaration, written back out as TessariQL.
//!
//! # The rule the whole module follows
//!
//! **Nothing is written on a guess.** Every function here returns the text or
//! names the part it could not write, and there is no third outcome — no
//! approximation, no elision, no placeholder. A declaration that *nearly*
//! re-creates a table is the worst answer available: it parses, it runs, and it
//! leaves a table that differs from the one it claimed to describe, which is a
//! difference nobody looks for because the script came from the store itself.
//!
//! That is why [`literal`] refuses most of the value system. A value's
//! `Display` is a summary written for a person — `<array of 2>`, `<3 bytes>`,
//! `90.000000000s` — and running any of those through a parser gives either a
//! refusal or, worse, a different value. Only the spellings this module can
//! prove re-read to the value they came from are written; the rest name
//! themselves and the declaration is withheld.
//!
//! # Why the text is built here rather than by the renderer
//!
//! `tessari_ql::render` refuses every declaration by name — `DEFINE TABLE`,
//! `DEFINE COLLECTION`, `DEFINE INDEX`, `DEFINE FIELD` all return
//! `unrenderable`. That renderer serves the query builder, which builds reads
//! and binds its literals as parameters, so it has never needed to write a
//! literal down. This module needs exactly that, and it reads the catalog to do
//! it, so it lives on the catalog's side of the wall.
//!
//! # One shape for all four words
//!
//! The declaration first, then one `DEFINE FIELD` per field, then one
//! `DEFINE INDEX` per index — for a table, a collection and a bucket alike. The
//! columnar spelling `DEFINE TABLE t (a string)` is not used, because a
//! collection cannot take a column list at all and the columnar form would
//! therefore need a second path for exactly one of the four words.

mod parts;
use std::fmt::Write as _;

pub(crate) use parts::{write_event, write_field, write_index, write_split};
use tessari_storage::{
    EDGE_IN, EDGE_OUT, FieldDefinition, GEO_FIELD, IndexDefinition, TableDefinition, TableKind,
    VECTOR_FIELD,
};
use tessari_types::{IdentityKind, Number};

/// The part of a declaration that had no faithful spelling.
///
/// A named part rather than a bare `None`, because a report that says *this
/// table has no definition* invites the reader to conclude the table is simple.
/// The truth is that one field's assertion, or one index's kind, is outside what
/// this module can write down, and naming it is what lets somebody fix it.
pub(crate) struct Unwritable {
    /// What could not be written, as it goes into the report.
    pub(crate) part: String,
}

impl Unwritable {
    fn at(part: impl Into<String>) -> Self {
        Self { part: part.into() }
    }
}

/// The script that re-creates this table, its fields and its indexes.
///
/// The fields and indexes are taken as given rather than read here, so that the
/// caller passes the **same** lists it put in the report. A second read could
/// return a different set — a concurrent `DEFINE FIELD`, or the caller's own
/// field grant applied in one place and not the other — and a definition that
/// disagreed with the report beside it would be two answers to one question.
///
/// # Errors
///
/// Returns [`Unwritable`] naming the first part with no faithful spelling.
pub(crate) fn declaration(
    definition: &TableDefinition,
    fields: &[FieldDefinition],
    indexes: &[IndexDefinition],
) -> Result<String, Unwritable> {
    let mut script = String::new();
    write_table(&mut script, definition)?;
    // A vector store's field and index are written by the word that declared
    // them, so writing them again would refuse on re-execution with the name
    // already taken. This is the one place a table's parts are not all written:
    // for every other kind the declaration and its parts are separate
    // statements, and here the word is all three.
    let declared_by_the_word = match definition.kind {
        TableKind::Vector(_) => Some(VECTOR_FIELD),
        TableKind::Geo => Some(GEO_FIELD),
        _ => None,
    };
    for field in fields {
        if declared_by_the_word == Some(field.name.as_str())
            || made_by_the_edge_word(definition, &field.name)
        {
            continue;
        }
        write_field(&mut script, &definition.name, field)?;
    }
    for index in indexes {
        if declared_by_the_word == Some(index.name.as_str())
            || made_by_the_edge_word(definition, &index.name)
            || index.engine.is_some()
        {
            continue;
        }
        write_index(&mut script, &definition.name, index)?;
    }
    Ok(script)
}

/// Whether `name` is an endpoint field or index `DEFINE TABLE … EDGE` makes for
/// itself.
///
/// Written out again they refuse on re-execution — the name is already taken —
/// so a declaration that wrote them did not re-create the table it described.
/// That was true of `INFO FOR TABLE` on every edge table until a state script,
/// which runs the declarations it writes, found it (G047).
pub(crate) fn made_by_the_edge_word(definition: &TableDefinition, name: &str) -> bool {
    matches!(definition.kind, TableKind::Edge(_))
        && [EDGE_OUT, EDGE_IN]
            .iter()
            .any(|endpoint| name == *endpoint || name == format!("{endpoint}_edges"))
}

/// The statement that declares the table itself.
///
/// The strictness word is **always** written, even where the default would
/// supply it. This goal has already moved that default once — a declaration
/// leaning on it is one whose meaning changes when it moves again, and changes
/// silently, in a script somebody kept.
fn write_table(script: &mut String, definition: &TableDefinition) -> Result<(), Unwritable> {
    let name = &definition.name;
    // A declared pair names its endpoints by **table id**, and this writer is
    // handed a definition and no way to resolve one to a name. Written as the
    // bare `DEFINE TABLE … EDGE` it would parse, run, and quietly produce a
    // table that accepts every `RELATE` the declaration refuses — a script that
    // restores something weaker than what it was taken from, with nothing at any
    // point reporting a loss. Refused instead, by the mechanism this module
    // already has for a part with no faithful spelling.
    if definition.edge_endpoints().is_some() {
        return Err(Unwritable::at(format!(
            "edge table `{name}` declares endpoints this writer cannot name"
        )));
    }
    // A membership is stored as a graph id and refuses for exactly the same
    // reason, one clause further on: written without the `IN` clause the script
    // would restore a table that belongs to no graph, so `INFO FOR GRAPH` would
    // no longer list it and every walk bounded by that graph would quietly stop
    // reaching it.
    //
    // This is before the per-kind arms and catches **every** kind, including a
    // queue that now says `IN` for itself. That is uniform rather than a queue's
    // problem: a plain `DEFINE TABLE staff SCHEMAFULL IN work` has been
    // undefinable for the same reason since graphs arrived, because this writer
    // is handed a `GraphId` and nothing that resolves one to a name. Lifting it
    // is a change to what this function is given, not to this arm (Q-521).
    if definition.graph.is_some() {
        return Err(Unwritable::at(format!(
            "table `{name}` belongs to a graph this writer cannot name"
        )));
    }
    // A queue is written back as the word that declared it, and until W157 it was
    // not written back at all: with no arm here it fell through to the tail
    // below and described itself as `DEFINE TABLE jobs SCHEMALESS` — a statement
    // that re-executes happily and restores a table with two ordinary fields and
    // no hold, so every refusal the word carries is gone and `CLAIM` no longer
    // works against it. Nothing was in an error state, which is the exact
    // failure this module exists to prevent (Q-475).
    //
    // The arm is a statement rather than an `Unwritable` refusal because a
    // queue's declaration is fully sayable: a `Duration` and an `Option<u32>`
    // are both literals the grammar reads back. The vault is the other case and
    // refuses, because a key is not a declaration.
    //
    // Strictness left this refusal in W208b¹, when `DEFINE QUEUE` learned to say
    // it. What remains is the edge clause, kept as a ratchet rather than as a
    // live case: `DEFINE QUEUE` has no `EDGE` word, so a queue cannot be an edge
    // table today — and if one ever can, the refusal is what stops this arm
    // writing a declaration that silently drops the endpoints.
    if let TableKind::Queue(declared) = &definition.kind {
        if definition.is_edge() {
            return Err(Unwritable::at(format!(
                "queue `{name}` carries flags its declaring word cannot say"
            )));
        }
        if definition.identity != IdentityKind::default() {
            return Err(Unwritable::at(format!(
                "queue `{name}` names records in a way its declaring word cannot say"
            )));
        }
        let _ = write!(
            script,
            "DEFINE QUEUE {name} TIMEOUT {}",
            declared.timeout.to_literal()
        );
        // Omitted when none was declared, because leaving the clause out is how
        // "unlimited" is spelled — and `ATTEMPTS 0` is refused by the grammar,
        // so writing the absent ceiling as a number would produce a statement
        // that does not parse rather than one that restores something weaker.
        if let Some(ceiling) = declared.attempts {
            let _ = write!(script, " ATTEMPTS {ceiling}");
        }
        // The two orderings a claim follows (G055 C8), written when declared.
        if let Some(field) = &declared.priority {
            let _ = write!(script, " PRIORITY BY {field}");
        }
        if let Some(field) = &declared.not_before {
            let _ = write!(script, " NOT BEFORE {field}");
        }
        // Always written, on this function's own rule: a declaration leaning on
        // a default is one whose meaning changes when the default moves, and
        // changes silently, in a script somebody kept.
        script.push_str(if definition.schemafull {
            " SCHEMAFULL"
        } else {
            " SCHEMALESS"
        });
        script.push_str(";\n");
        return Ok(());
    }
    // A series is written back for the reason a queue is: its whole declaration
    // is a duration, which the grammar reads back. Unlike a queue it says
    // nothing about its identity, because the kind fixes that — so a stored
    // series naming records any other way is a definition this word cannot
    // restore, and saying so is better than writing a statement that would
    // recreate it wrongly.
    // A space is written back with its own word and its limit (G036). It holds
    // single values, so a schema flag or an edge pair is something the word
    // cannot say, exactly as for a series.
    if let TableKind::Space(declared) = &definition.kind {
        if definition.schemafull || definition.is_edge() {
            return Err(Unwritable::at(format!(
                "space `{name}` carries flags its declaring word cannot say"
            )));
        }
        let _ = writeln!(
            script,
            "DEFINE SPACE {name}{};",
            crate::kv::space_clause(declared)
        );
        return Ok(());
    }
    // A topic the same way, with its clauses (G037).
    if let TableKind::Topic(declared) = &definition.kind {
        if definition.schemafull || definition.is_edge() {
            return Err(Unwritable::at(format!(
                "topic `{name}` carries flags its declaring word cannot say"
            )));
        }
        let _ = writeln!(
            script,
            "DEFINE TOPIC {name}{};",
            crate::topic::topic_clauses(declared)
        );
        return Ok(());
    }
    if let TableKind::Series(declared) = &definition.kind {
        // A rollup is declared from its series by `DEFINE ROLLUP`, which names
        // the series; this writer sees one table and cannot spell that — and
        // writing it back as a plain series would restore a table nothing keeps.
        if declared.rollup_of.is_some() {
            return Err(Unwritable::at(format!(
                "`{name}` is a rollup, declared with `DEFINE ROLLUP` on its series"
            )));
        }
        if definition.schemafull || definition.is_edge() {
            return Err(Unwritable::at(format!(
                "series `{name}` carries flags its declaring word cannot say"
            )));
        }
        if definition.identity != IdentityKind::Uuid {
            return Err(Unwritable::at(format!(
                "series `{name}` names records in a way its declaring word cannot say"
            )));
        }
        let time = declared
            .time
            .as_ref()
            .map_or_else(String::new, |field| format!(" TIME {field}"));
        let _ = writeln!(
            script,
            "DEFINE SERIES {name} RETAIN {}{time};",
            declared.retain.to_literal()
        );
        return Ok(());
    }
    // A view is written back as the statement it was declared with, and that
    // is exact rather than approximate: the read is stored as the text somebody
    // typed, so this is the one word here that restores the original character
    // for character. Everything the other arms refuse to write — a flag the word
    // cannot say, an identity it cannot express — a view does not have, because
    // it declares no fields, holds no records and names them in no way at all.
    if let TableKind::View(declared) = &definition.kind {
        if definition.schemafull || definition.is_edge() {
            return Err(Unwritable::at(format!(
                "view `{name}` carries flags its declaring word cannot say"
            )));
        }
        // A kept view is declared again as one and rebuilt from its source when
        // the script runs; its rows are not written (`script.rs`).
        let kept = if declared.materialized {
            " MATERIALIZED"
        } else {
            ""
        };
        let _ = writeln!(script, "DEFINE VIEW {name}{kept} AS {};", declared.read);
        return Ok(());
    }
    // Written back as the word that created it, which is the whole reason the
    // kind is stored rather than inferred from the field and the index it
    // creates: `DEFINE TABLE embeddings SCHEMALESS` re-executes happily and
    // restores a store that no longer knows the three belong together.
    if let TableKind::Vector(declared) = &definition.kind {
        if definition.schemafull || definition.is_edge() {
            return Err(Unwritable::at(format!(
                "table `{name}` carries flags its declaring word cannot say"
            )));
        }
        if definition.identity != IdentityKind::default() {
            return Err(Unwritable::at(format!(
                "vector store `{name}` names records in a way its declaring word cannot say"
            )));
        }
        let _ = writeln!(
            script,
            "DEFINE VECTOR {name} DIMENSION {} DISTANCE {};",
            declared.dimension,
            declared.distance.name()
        );
        return Ok(());
    }
    // Same contract, one word further on. A geo store carries no clause, so
    // there is nothing to write after the name — and the same three flags are
    // refused, because the word cannot say them either.
    if definition.kind == TableKind::Geo {
        if definition.schemafull || definition.is_edge() {
            return Err(Unwritable::at(format!(
                "table `{name}` carries flags its declaring word cannot say"
            )));
        }
        if definition.identity != IdentityKind::default() {
            return Err(Unwritable::at(format!(
                "geo store `{name}` names records in a way its declaring word cannot say"
            )));
        }
        let _ = writeln!(script, "DEFINE GEO {name};");
        return Ok(());
    }
    if definition.is_vault() {
        // `DEFINE VAULT` takes no flags, and a vault is strict by construction
        // rather than by a word anybody wrote — so a stored vault saying
        // otherwise was reached by a route this module does not know about, and
        // is refused rather than written out as a statement that would restore
        // something weaker than what was described.
        if !definition.schemafull || definition.is_edge() {
            return Err(Unwritable::at(format!(
                "vault `{name}` carries flags its declaring word cannot say"
            )));
        }
        // Written without a key, because a key is not a declaration: re-running
        // this mints a fresh one, and the vault it restores is an empty vault
        // rather than a second way into the first. That is the same promise
        // every other word here makes — the schema comes back, the data does
        // not — and it is worth stating because for this word a reader might
        // hope for more.
        let _ = writeln!(script, "DEFINE VAULT {name};");
        return Ok(());
    }
    if definition.is_bucket() || definition.is_collection() {
        // Neither word takes a flag, so neither can express a table that has
        // one. `DEFINE BUCKET` and `DEFINE COLLECTION` both store
        // `schemafull: false, edge: false`, and a stored table that says
        // otherwise was reached by a route this module does not know about.
        if definition.schemafull || definition.is_edge() {
            return Err(Unwritable::at(format!(
                "table `{name}` carries flags its declaring word cannot say"
            )));
        }
        // `DEFINE BUCKET` takes no `IDENTITY`, and a bucket names its records
        // itself, so one storing anything but the default was reached by a route
        // this module does not know about — the same judgement as the flags
        // above, and refused the same way rather than written out as a statement
        // that would not parse.
        if definition.is_bucket() && definition.identity != IdentityKind::default() {
            return Err(Unwritable::at(format!(
                "bucket `{name}` names records in a way its declaring word cannot say"
            )));
        }
        let word = if definition.is_bucket() {
            "BUCKET"
        } else {
            "COLLECTION"
        };
        let _ = write!(script, "DEFINE {word} {name}");
        // Without this the script re-executes happily and the bucket comes back
        // unbounded — the failure a round trip exists to catch, and a silent one
        // because nothing about the restored store is in an error state until a
        // file the original would have refused is accepted.
        if let Some(ceiling) = definition.byte_ceiling() {
            let _ = write!(script, " MAX {ceiling}");
        }
        if !definition.is_bucket() {
            write_identity(script, definition);
        }
        script.push_str(";\n");
        return Ok(());
    }
    let _ = write!(script, "DEFINE TABLE {name}");
    if definition.is_edge() {
        script.push_str(" EDGE");
    }
    script.push_str(if definition.schemafull {
        " SCHEMAFULL"
    } else {
        " SCHEMALESS"
    });
    write_identity(script, definition);
    write_conflict(script, definition);
    write_split(script, definition);
    script.push_str(";\n");
    Ok(())
}

/// What the table does with a write it cannot order, written only when the
/// author said it.
///
/// Unlike the strictness word and the naming scheme, this is **not** written
/// when it was never declared. Those two are always emitted because their
/// defaults can MOVE between builds, so a declaration that omitted them would
/// quietly mean something else on the next one. A silent conflict policy cannot
/// drift that way: ADR-0075 defines the absence itself as a refusal, and it is
/// the refusal the whole goal exists to make. Emitting `REFUSE CONFLICTS` here
/// anyway would put a word into every existing table's declaration that its
/// author did not write, which is a different claim about what was declared.
///
/// Written at all because without it a table that declares `LAST WRITER WINS`
/// describes itself as one that refuses: the report and the declaration rebuilt
/// from it would be identical for two tables whose writes behave differently,
/// and a schema round trip would compare two agreeing reports having produced
/// the wrong table.
fn write_conflict(script: &mut String, definition: &TableDefinition) {
    if let Some(policy) = definition.conflict {
        let _ = write!(script, " {policy}");
    }
}

/// The naming scheme, written for the same reason the strictness word is.
///
/// Always, never left to the default. A declaration that omitted it would keep
/// meaning what the build it was taken from meant, and would quietly mean
/// something else on a build whose default had moved — and here the difference
/// is not what a table *accepts* but what every record written to it is
/// *called*, which no later read can undo.
fn write_identity(script: &mut String, definition: &TableDefinition) {
    let _ = write!(script, " IDENTITY {}", definition.identity);
    // And so is the bucket a spread identity begins with (ADR-0113 D1).
    if definition.spread {
        script.push_str(" SPREAD");
    }
    // Part of the naming scheme too: a table restored without it would go on
    // accepting records whose identity and region disagree (ADR-0096).
    if let Some(field) = &definition.partition {
        let _ = write!(script, " PARTITION BY {field}");
    }
}

/// A number as the literal that produces it.
///
/// A float is written with the debug formatting rather than the display one,
/// because display writes `3` for three and the lexer reads that back as an
/// integer — a different value of a different type, arriving silently. Debug
/// writes the shortest text that parses back to the same float, which is the
/// property this needs.
///
/// A decimal has no literal at all: the lexer produces integers and floats and
/// nothing else, so a decimal in the catalog was cast into place and no text
/// re-creates it.
fn number_literal(number: &Number) -> Option<String> {
    match number {
        Number::Integer(held) => Some(held.to_string()),
        Number::Float(held) if held.is_finite() => Some(format!("{held:?}")),
        Number::Float(_) | Number::Decimal(_) => None,
    }
}
