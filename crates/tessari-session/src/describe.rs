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

use std::fmt::Write as _;

use tessari_storage::{
    FieldDefinition, GEO_FIELD, IndexDefinition, TableDefinition, TableKind, VECTOR_FIELD,
};
use tessari_types::{Assertion, IdentityKind, Number, Operand, Value};

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
        if declared_by_the_word == Some(field.name.as_str()) {
            continue;
        }
        write_field(&mut script, &definition.name, field)?;
    }
    for index in indexes {
        if declared_by_the_word == Some(index.name.as_str()) {
            continue;
        }
        write_index(&mut script, &definition.name, index)?;
    }
    Ok(script)
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
    if let TableKind::Series(declared) = &definition.kind {
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
        let _ = writeln!(
            script,
            "DEFINE SERIES {name} RETAIN {};",
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
        let _ = writeln!(script, "DEFINE VIEW {name} AS {};", declared.read);
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
}

/// One field's declaration.
///
/// `DEFAULT` needs no rendering: the catalog stores what the author typed, so
/// the text goes back exactly as it came.
fn write_field(
    script: &mut String,
    table: &str,
    field: &FieldDefinition,
) -> Result<(), Unwritable> {
    let name = &field.name;
    let _ = write!(
        script,
        "DEFINE FIELD {name} ON {table} TYPE {}",
        field.kind.name()
    );
    // First among the options and never omitted. A field declared `SECRET` and
    // written back without the word restores as an ordinary field, so a schema
    // round trip through this module would un-seal every secret a vault holds —
    // silently, because the restored store is in no error state and the field
    // still carries the name, the type and the value the caller expects.
    if field.secret {
        script.push_str(" SECRET");
    }
    if field.required {
        script.push_str(" REQUIRED");
    }
    if let Some(default) = &field.default {
        let _ = write!(script, " DEFAULT {default}");
    }
    if let Some(analyzer) = &field.analyzer {
        let _ = write!(script, " ANALYZER {analyzer}");
    }
    if let Some(assert) = &field.assert {
        let written = assertion(assert)
            .ok_or_else(|| Unwritable::at(format!("the assertion on field `{name}`")))?;
        let _ = write!(script, " ASSERT {written}");
    }
    script.push_str(";\n");
    Ok(())
}

/// One index's declaration.
///
/// At most one kind word, because the grammar accepts at most one: `UNIQUE`,
/// `SEARCH`, `SPATIAL` and `VECTOR <distance>` are four index kinds and a
/// statement naming two is refused where it is written.
fn write_index(
    script: &mut String,
    table: &str,
    index: &IndexDefinition,
) -> Result<(), Unwritable> {
    let name = &index.name;
    let projected = index
        .fields
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let _ = write!(script, "DEFINE INDEX {name} ON {table} FIELDS {projected}");
    match (index.unique, index.search, index.spatial, index.vector) {
        (false, false, false, None) => {}
        (true, false, false, None) => script.push_str(" UNIQUE"),
        (false, true, false, None) => script.push_str(" SEARCH"),
        (false, false, true, None) => script.push_str(" SPATIAL"),
        (false, false, false, Some(distance)) => {
            let _ = write!(script, " VECTOR {}", distance.name());
        }
        // Two kinds at once is a state the grammar refuses and the catalog
        // should not hold. Withholding the declaration is the only honest
        // answer: writing one of the two would describe a different index.
        _ => {
            return Err(Unwritable::at(format!(
                "index `{name}` carries more than one kind"
            )));
        }
    }
    script.push_str(";\n");
    Ok(())
}

/// A constraint, written the way its author wrote it.
///
/// `All` and `Any` are refused with anything other than two parts. The parser
/// lowers `a AND b` into a pair and nests for a third, so every constraint it
/// produces is binary — and a flat list of three would have to be written as
/// `(a AND b AND c)`, which re-reads as a pair whose first half is a pair. That
/// is a different constraint carrying the same words, which is exactly the class
/// of answer this module exists to withhold.
fn assertion(constraint: &Assertion) -> Option<String> {
    match constraint {
        Assertion::Compare { op, against } => {
            let right = match against {
                Operand::Literal(held) => literal(held)?,
                Operand::Field(route) => route.to_string(),
            };
            Some(format!("$value {} {right}", op.spelling()))
        }
        Assertion::All(parts) => pair(parts, "AND"),
        Assertion::Any(parts) => pair(parts, "OR"),
        Assertion::Not(inner) => Some(format!("(NOT {})", assertion(inner)?)),
    }
}

fn pair(parts: &[Assertion], word: &str) -> Option<String> {
    let [left, right] = parts else {
        return None;
    };
    Some(format!(
        "({} {word} {})",
        assertion(left)?,
        assertion(right)?
    ))
}

/// A value as the literal that produces it, or nothing.
///
/// The set is small on purpose and every member of it is a spelling the lexer
/// reads back into the value it came from. The rest are absent for one of two
/// reasons, and both are reasons to withhold rather than to approximate:
///
/// - **No literal exists.** There is no datetime, uuid, object, range,
///   geometry or regex literal in the language, so a value of those kinds
///   reached the catalog through a cast and nothing written here would parse
///   back into it.
/// - **A literal exists and this module cannot prove its spelling.** A duration
///   is written `1h30m` and displayed `90.000000000s`, which lexes as a float
///   and a stray name. A set has a literal form; writing one without a test
///   proving it re-reads would be exactly the guess this module refuses.
fn literal(value: &Value) -> Option<String> {
    match value {
        Value::None => Some("NONE".to_owned()),
        Value::Null => Some("NULL".to_owned()),
        Value::Bool(held) => Some(held.to_string()),
        Value::Number(number) => number_literal(number),
        Value::String(text) => quoted(text),
        Value::Array(items) => {
            let mut written = Vec::with_capacity(items.len());
            for item in items {
                written.push(literal(item)?);
            }
            Some(format!("[{}]", written.join(", ")))
        }
        Value::Bytes(_)
        | Value::Duration(_)
        | Value::Datetime(_)
        | Value::Uuid(_)
        | Value::Table(_)
        | Value::Record(_)
        | Value::Object(_)
        | Value::Range(_)
        | Value::Set(_)
        | Value::Geometry(_)
        | Value::Regex(_) => None,
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

/// Text as a single-quoted literal.
///
/// Only the escapes the lexer accepts are written, and a control character it
/// has no escape for makes the whole literal **absent** rather than a string
/// carrying a raw byte. Passing one through would either fail to lex or lex as
/// something else depending on where it sat, and a script that fails to parse
/// on the one field nobody tested is the same defect as one that parses wrong.
fn quoted(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('\'');
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            other if other.is_control() => return None,
            other => out.push(other),
        }
    }
    out.push('\'');
    Some(out)
}
