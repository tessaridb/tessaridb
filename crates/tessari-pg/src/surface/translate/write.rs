//! The write statements the dialect's clients send over the wire, mapped
//! onto the store's own writes: INSERT rides CREATE, UPDATE and DELETE
//! ride their set forms, and every write carries the store's RETURN so the
//! wire owes an honest count. CREATE TABLE and DROP TABLE map onto the
//! catalog's DEFINE/DROP with the type correspondence inverted.
//!
//! What the dialect allows but this build does not speak refuses by name:
//! INSERT without a column list, several VALUES rows, writes without their
//! WHERE, constraints and defaults in CREATE TABLE, and everything past
//! the mapped subset.

use super::parser::Parser;
use super::{Columns, Refused, Shaped, code};

/// The completion tag the wire answers a statement under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tag {
    Select,
    Insert,
    Update,
    Delete,
    CreateTable,
    DropTable,
    /// A transaction verb running as a store statement of its own — the
    /// simple query's explicit `BEGIN … COMMIT` block.
    Begin,
    Commit,
    Rollback,
}

impl Tag {
    /// The completion word, with the row count the dialect counts.
    pub(crate) fn completion(self, count: usize) -> String {
        match self {
            Tag::Select => format!("SELECT {count}"),
            Tag::Insert => format!("INSERT 0 {count}"),
            Tag::Update => format!("UPDATE {count}"),
            Tag::Delete => format!("DELETE {count}"),
            Tag::CreateTable => "CREATE TABLE".to_owned(),
            Tag::DropTable => "DROP TABLE".to_owned(),
            Tag::Begin => "BEGIN".to_owned(),
            Tag::Commit => "COMMIT".to_owned(),
            Tag::Rollback => "ROLLBACK".to_owned(),
        }
    }
}

/// Whether the text opens a statement this module speaks.
pub(crate) fn opens(first: &str) -> bool {
    matches!(
        first.to_ascii_lowercase().as_str(),
        "insert" | "update" | "delete" | "create" | "drop"
    )
}

/// Translate one write statement — the extended protocol's Parse, exactly
/// one per call.
pub(crate) fn translate(query: &str) -> Result<super::Translated, Refused> {
    let mut parser = Parser::new(query);
    let shaped = statement(&mut parser)?;
    parser.end_write()?;
    Ok(finish(parser, vec![shaped]))
}

/// Translate a simple query's write statements, several per text, the block
/// the statement layer wraps around them.
pub(crate) fn translate_many(query: &str) -> Result<super::Translated, Refused> {
    let mut parser = Parser::new(query);
    let mut statements = vec![statement(&mut parser)?];
    while parser.end_more()? {
        statements.push(statement(&mut parser)?);
    }
    Ok(finish(parser, statements))
}

/// The parser's collected state moves into the translation.
fn finish(parser: Parser<'_>, statements: Vec<Shaped>) -> super::Translated {
    super::Translated {
        script: statements
            .iter()
            .map(|shaped| shaped.script.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        parameters: parser.parameters,
        statements,
        slots: parser.slots,
    }
}

/// One write statement at the cursor. `pub(crate)` for the `verbs`
/// dispatcher, which mixes writes with the other statements on one parser.
pub(crate) fn statement(parser: &mut Parser<'_>) -> Result<Shaped, Refused> {
    if parser.keyword("INSERT").is_ok() {
        return insert(parser);
    }
    if parser.keyword("UPDATE").is_ok() {
        return update(parser);
    }
    if parser.keyword("DELETE").is_ok() {
        return delete(parser);
    }
    if parser.keyword("CREATE").is_ok() {
        return create_table(parser);
    }
    if parser.keyword("DROP").is_ok() {
        return drop_table(parser);
    }
    Err(Refused::surface(
        code::SYNTAX,
        "a write starts with INSERT, UPDATE, DELETE, CREATE TABLE or DROP TABLE in this build",
    ))
}

/// `INSERT INTO t (cols…) VALUES (values…) [RETURNING …]`: one row per
/// statement in this build — the store's CREATE owns the identity, the
/// column list says the shape out loud.
fn insert(parser: &mut Parser<'_>) -> Result<Shaped, Refused> {
    if parser.keyword("INTO").is_err() {
        return Err(Refused::surface(code::SYNTAX, "INSERT wants INTO"));
    }
    let table = parser.ident()?;
    if !parser.open_paren() {
        return Err(Refused::surface(
            code::FEATURE,
            "INSERT names its columns in this build; a bare INSERT is not spoken",
        ));
    }
    let mut columns = vec![parser.ident()?];
    while parser.comma().is_ok() {
        columns.push(parser.ident()?);
    }
    parser.close_paren()?;
    if parser.keyword("VALUES").is_err() {
        return Err(Refused::surface(code::SYNTAX, "INSERT wants VALUES"));
    }
    if !parser.open_paren() {
        return Err(Refused::surface(code::SYNTAX, "VALUES wants parentheses"));
    }
    let mut values = vec![value(parser)?];
    while parser.comma().is_ok() {
        values.push(value(parser)?);
    }
    parser.close_paren()?;
    if parser.operator(",") {
        return Err(Refused::surface(
            code::FEATURE,
            "one VALUES row per INSERT is spoken in this build; a batch is not",
        ));
    }
    if values.len() != columns.len() {
        return Err(Refused::with_detail(
            code::SYNTAX,
            "INSERT wants one value per column",
            format!("{} columns, {} values", columns.len(), values.len()),
        ));
    }
    let returning = returning(parser)?;
    let fields = columns
        .iter()
        .zip(values.iter())
        .map(|(column, value)| format!("{column}: {value}"))
        .collect::<Vec<_>>()
        .join(", ");
    // The store's CREATE owns the identity; RETURN rides so the count is
    // honest, the wire answers the tag with or without the rows.
    let script = format!("CREATE {table} = {{ {fields} }} RETURN AFTER;");
    let shaped = shape(parser, Tag::Insert, script, returning)?;
    Ok(shaped)
}

/// `UPDATE t SET col = value, … WHERE cond [RETURNING …]` — the store's
/// set UPDATE; the write states its reach through the WHERE, as the store
/// counts a write's reach.
fn update(parser: &mut Parser<'_>) -> Result<Shaped, Refused> {
    let table = parser.ident()?;
    if parser.keyword("SET").is_err() {
        return Err(Refused::surface(code::SYNTAX, "UPDATE wants SET"));
    }
    let mut sets = String::new();
    let mut inference = Vec::new();
    loop {
        let column = parser.ident()?;
        if !parser.operator("=") {
            return Err(Refused::surface(
                code::SYNTAX,
                "SET wants = and the value to write",
            ));
        }
        // `SET col = $n` tells Parse-time inference which column the
        // parameter's type rides on, as a WHERE comparison does.
        let at = parser.snapshot();
        let value = if let Some(held) = parser.placeholder() {
            if let Some(slot) = Parser::slot_of(&held)
                && !inference.iter().any(|(held_slot, _)| *held_slot == slot)
            {
                inference.push((slot, column.clone()));
            }
            held
        } else {
            parser.restore(at);
            value(parser)?
        };
        if !sets.is_empty() {
            sets.push_str(", ");
        }
        sets.push_str(&format!("{column} = {value}"));
        if parser.comma().is_err() {
            break;
        }
    }
    if parser.keyword("WHERE").is_err() {
        return Err(Refused::surface(
            code::FEATURE,
            "UPDATE states its reach with WHERE in this build; a whole-table UPDATE is not spoken",
        ));
    }
    let condition = parser.or()?;
    let returning = returning(parser)?;
    let script = format!("UPDATE {table} SET {sets} WHERE {condition} RETURN AFTER;");
    let mut shaped = shape(parser, Tag::Update, script, returning)?;
    shaped.inference = inference;
    shaped.inference.append(&mut parser.inference);
    shaped.table = Some(table);
    Ok(shaped)
}

/// `DELETE FROM t WHERE cond [RETURNING …]` — the deleted rows are the
/// before shape, which is what the dialect's RETURNING reads for a delete.
fn delete(parser: &mut Parser<'_>) -> Result<Shaped, Refused> {
    if parser.keyword("FROM").is_err() {
        return Err(Refused::surface(code::SYNTAX, "DELETE wants FROM"));
    }
    let table = parser.ident()?;
    if parser.keyword("WHERE").is_err() {
        return Err(Refused::surface(
            code::FEATURE,
            "DELETE states its reach with WHERE in this build; a whole-table DELETE is not spoken",
        ));
    }
    let condition = parser.or()?;
    let returning = returning(parser)?;
    let script = format!("DELETE FROM {table} WHERE {condition} RETURN BEFORE;");
    let mut shaped = shape(parser, Tag::Delete, script, returning)?;
    shaped.table = Some(table);
    Ok(shaped)
}

/// `CREATE TABLE t (col TYPE, …)`: the type correspondence inverted, one
/// map both directions. Constraints, defaults and keys refuse by name.
fn create_table(parser: &mut Parser<'_>) -> Result<Shaped, Refused> {
    if parser.keyword("TABLE").is_err() {
        return Err(Refused::surface(
            code::SYNTAX,
            "CREATE wants TABLE in this build",
        ));
    }
    let table = parser.ident()?;
    let mut if_not_exists = false;
    if parser.keyword("IF").is_ok() {
        if parser.keyword("NOT").is_err() || parser.keyword("EXISTS").is_err() {
            return Err(Refused::surface(code::SYNTAX, "IF wants NOT EXISTS"));
        }
        if_not_exists = true;
    }
    if !parser.open_paren() {
        return Err(Refused::surface(
            code::SYNTAX,
            "CREATE TABLE wants its columns in parentheses",
        ));
    }
    let mut defines = String::new();
    loop {
        let column = parser.ident()?;
        let kind = field_kind(parser)?;
        defines.push_str(&format!("DEFINE FIELD {column} ON {table} TYPE {kind}; "));
        if parser.comma().is_err() {
            break;
        }
    }
    parser.close_paren()?;
    let exists = if if_not_exists { " IF NOT EXISTS" } else { "" };
    let script = format!("DEFINE TABLE{exists} {table} SCHEMALESS; {defines}");
    shape(parser, Tag::CreateTable, script, None)
}

/// `DROP TABLE t [IF EXISTS]`.
fn drop_table(parser: &mut Parser<'_>) -> Result<Shaped, Refused> {
    if parser.keyword("TABLE").is_err() {
        return Err(Refused::surface(
            code::SYNTAX,
            "DROP wants TABLE in this build",
        ));
    }
    let table = parser.ident()?;
    let exists = if parser.keyword("IF").is_ok() {
        if parser.keyword("EXISTS").is_err() {
            return Err(Refused::surface(code::SYNTAX, "IF wants EXISTS"));
        }
        " IF EXISTS"
    } else {
        ""
    };
    let script = format!("DROP TABLE{exists} {table};");
    shape(parser, Tag::DropTable, script, None)
}

/// The store field kind a dialect column type maps onto; anything outside
/// the correspondence refuses by name rather than guessing.
fn field_kind(parser: &mut Parser<'_>) -> Result<&'static str, Refused> {
    let first = type_word(parser)?;
    let kind = match (first.as_str(), second_word(parser)) {
        ("int" | "integer" | "int2" | "smallint" | "int4" | "int8" | "bigint", None) => "int",
        ("text" | "varchar" | "char" | "character" | "string", None) => "string",
        ("bool" | "boolean", None) => "bool",
        ("float" | "real" | "float4" | "float8", None) => "float",
        ("double", Some(second)) if second == "precision" => "float",
        ("numeric" | "decimal", None) => "decimal",
        ("date" | "datetime" | "timestamp" | "timestamptz", None) => "datetime",
        ("uuid", None) => "uuid",
        ("json" | "jsonb", None) => "object",
        ("bytea" | "bytes", None) => "bytes",
        _ => {
            return Err(Refused::with_detail(
                code::FEATURE,
                "this column type is not spoken in this build",
                first,
            ));
        }
    };
    Ok(kind)
}

/// One type word: the names the dialect spells are bare words.
fn type_word(parser: &mut Parser<'_>) -> Result<String, Refused> {
    parser
        .bare_ident()
        .ok_or_else(|| Refused::surface(code::SYNTAX, "a column type was expected"))
}

/// The second word of a two-word type (`double precision`), when it rides.
fn second_word(parser: &mut Parser<'_>) -> Option<String> {
    let at = parser.snapshot();
    parser.skip_space();
    let word = parser.bare_ident()?;
    if matches!(word.as_str(), "precision" | "varying") {
        Some(word)
    } else {
        parser.restore(at);
        None
    }
}

/// One VALUES or SET value: a literal binds, a placeholder passes as its
/// own text; a parenthesized or signed form refuses by name.
fn value(parser: &mut Parser<'_>) -> Result<String, Refused> {
    if let Some(held) = parser.placeholder() {
        return Ok(held);
    }
    if let Some(literal) = parser.literal()? {
        return Ok(parser.bind(literal));
    }
    Err(Refused::surface(
        code::FEATURE,
        "a write value wants a literal or a parameter in this build",
    ))
}

/// The RETURNING clause, when it rides: `*` names nothing the projection
/// can answer without running; a column list answers under those names.
fn returning(parser: &mut Parser<'_>) -> Result<Option<Columns>, Refused> {
    if parser.keyword("RETURNING").is_err() {
        return Ok(None);
    }
    if parser.star() {
        return Ok(Some(Columns::Star));
    }
    let mut names = vec![parser.ident()?];
    while parser.comma().is_ok() {
        names.push(parser.ident()?);
    }
    Ok(Some(Columns::Named(names)))
}

/// The shaped statement: the tag the wire answers under, the returning
/// projection, the store script.
fn shape(
    _parser: &Parser<'_>,
    tag: Tag,
    script: String,
    returning: Option<Columns>,
) -> Result<Shaped, Refused> {
    Ok(Shaped {
        script,
        columns: returning.clone().unwrap_or(Columns::Named(Vec::new())),
        table: None,
        catalog: None,
        inference: Vec::new(),
        record_names: Vec::new(),
        tag,
        returning,
    })
}
