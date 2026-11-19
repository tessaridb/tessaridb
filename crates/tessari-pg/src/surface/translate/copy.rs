//! The COPY statement: the dialect's bulk load in both directions. It runs
//! alone in its query string — the answer is the copy subprotocol (a
//! CopyIn/OutResponse, then CopyData frames), not the row flow — so the
//! simple path intercepts it before the usual translation, and the extended
//! path refuses it by name until its own slice. Text format is the one
//! spoken; every other format, direction and server-side file refuses by
//! name.

use super::parser::Parser;
use super::{Refused, code};

/// The COPY statement a simple query may hold.
#[derive(Debug)]
pub(crate) enum Statement {
    /// `COPY table [(cols…)] FROM STDIN` — text rows stream in and land as
    /// one engine INSERT.
    From {
        /// The table the rows land on.
        table: String,
        /// The named columns, empty reading as every declared field.
        columns: Vec<String>,
        /// The text-format options the statement named.
        options: Options,
    },
    /// `COPY {table [(cols…)] | (query)} TO STDOUT` — rows stream out.
    To {
        /// Where the rows come from.
        source: Source,
        /// The text-format options the statement named.
        options: Options,
    },
}

/// The source a COPY TO reads.
#[derive(Debug)]
pub(crate) enum Source {
    /// A table, with the named columns or every declared field.
    Table {
        /// The table the rows come from.
        table: String,
        /// The named columns, empty reading as every declared field.
        columns: Vec<String>,
    },
    /// `(SELECT …)` — the query text, translated by the usual SELECT path.
    Query(String),
}

/// The text-format options the statement may name; the defaults are the
/// dialect's own.
#[derive(Debug)]
pub(crate) struct Options {
    /// The column separator, a tab unless named.
    pub(crate) delimiter: u8,
    /// The null marker, `\N` unless named — matched before escapes run.
    pub(crate) null: String,
    /// Whether a header line rides: skipped on FROM, emitted on TO.
    pub(crate) header: bool,
}

/// Parse one query string as a COPY statement; `Ok(None)` hands the text to
/// the usual translators. A COPY runs alone: anything after its options is
/// the dialect's own punctuation and nothing more.
pub(crate) fn parse(query: &str) -> Result<Option<Statement>, Refused> {
    let mut parser = Parser::new(query);
    if parser.keyword("COPY").is_err() {
        return Ok(None);
    }
    let statement = copy(&mut parser)?;
    parser.end_write()?;
    Ok(Some(statement))
}

/// `COPY <source> FROM STDIN | TO STDOUT [WITH …]`.
fn copy(parser: &mut Parser<'_>) -> Result<Statement, Refused> {
    let source = if parser.open_paren() {
        Source::Query(parenthesised_query(parser)?)
    } else {
        let table = parser.ident()?;
        let mut columns = Vec::new();
        if parser.open_paren() {
            columns.push(parser.ident()?);
            while parser.comma().is_ok() {
                columns.push(parser.ident()?);
            }
            parser.close_paren()?;
        }
        Source::Table { table, columns }
    };
    if matches!(source, Source::Query(_)) && parser.open_paren() {
        return Err(Refused::surface(
            code::FEATURE,
            "a column list on the COPY query form is not spoken in this build",
        ));
    }
    if parser.keyword("FROM").is_ok() {
        if matches!(source, Source::Query(_)) {
            return Err(Refused::surface(
                code::SYNTAX,
                "COPY FROM wants a table in this build",
            ));
        }
        stream_word(parser, "FROM", "STDIN")?;
        let options = options(parser)?;
        let Source::Table { table, columns } = source else {
            unreachable!("the query form refused above")
        };
        return Ok(Statement::From {
            table,
            columns,
            options,
        });
    }
    if parser.keyword("TO").is_ok() {
        stream_word(parser, "TO", "STDOUT")?;
        let options = options(parser)?;
        return Ok(Statement::To { source, options });
    }
    Err(Refused::surface(code::SYNTAX, "COPY wants FROM or TO"))
}

/// The stream word after FROM/TO: STDIN or STDOUT as the direction says; the
/// swap, a server-side file and PROGRAM each refuse by name.
fn stream_word(parser: &mut Parser<'_>, word: &str, spoken: &str) -> Result<(), Refused> {
    if parser.keyword(spoken).is_ok() {
        return Ok(());
    }
    let swapped = if spoken == "STDIN" { "STDOUT" } else { "STDIN" };
    if parser.keyword(swapped).is_ok() {
        return Err(Refused::with_detail(
            code::FEATURE,
            format!("COPY {word} {swapped} is not spoken in this build"),
            "the direction swap",
        ));
    }
    if parser.keyword("PROGRAM").is_ok() || parser.literal()?.is_some() {
        return Err(Refused::surface(
            code::FEATURE,
            "COPY to or from a server-side file is not spoken in this build",
        ));
    }
    Err(Refused::surface(
        code::SYNTAX,
        format!("COPY {word} wants {spoken} in this build"),
    ))
}

/// `[WITH] [(] <option> [, …] [)]` — FORMAT text, DELIMITER 'x', NULL 's',
/// HEADER [bool]. Anything else refuses naming itself.
fn options(parser: &mut Parser<'_>) -> Result<Options, Refused> {
    let mut options = Options {
        delimiter: b'\t',
        null: "\\N".to_owned(),
        header: false,
    };
    let _ = parser.keyword("WITH");
    let parens = parser.open_paren();
    loop {
        match parser.peek_word().as_deref() {
            Some("format") => {
                parser.keyword("FORMAT").map_err(|_| format_err())?;
                let Some(format) = parser.peek_word() else {
                    return Err(Refused::surface(code::SYNTAX, "FORMAT wants a name"));
                };
                if format != "text" {
                    return Err(Refused::with_detail(
                        code::FEATURE,
                        format!("FORMAT {format} is not spoken in this build"),
                        "text is the one format spoken",
                    ));
                }
                let _ = parser.keyword("TEXT");
            }
            Some("delimiter") => {
                parser.keyword("DELIMITER").map_err(|_| format_err())?;
                let Some(tessaridb::Value::String(text)) = parser.literal()? else {
                    return Err(Refused::surface(code::SYNTAX, "DELIMITER wants a string"));
                };
                let mut bytes = text.bytes();
                match (bytes.next(), bytes.next()) {
                    (Some(delimiter), None) => options.delimiter = delimiter,
                    _ => {
                        return Err(Refused::surface(
                            code::FEATURE,
                            "COPY speaks one-byte delimiters in this build",
                        ));
                    }
                }
            }
            Some("null") => {
                parser.keyword("NULL").map_err(|_| format_err())?;
                let Some(tessaridb::Value::String(text)) = parser.literal()? else {
                    return Err(Refused::surface(code::SYNTAX, "NULL wants a string"));
                };
                options.null = text;
            }
            Some("header") => {
                parser.keyword("HEADER").map_err(|_| format_err())?;
                match parser.peek_word().as_deref() {
                    Some("true") | Some("on") => {
                        let _ = parser.bare_ident();
                        options.header = true;
                    }
                    Some("false") | Some("off") => {
                        let _ = parser.bare_ident();
                    }
                    // A bare HEADER is the dialect's own true.
                    _ => options.header = true,
                }
            }
            Some(other) => {
                return Err(Refused::with_detail(
                    code::FEATURE,
                    format!("the COPY option {other} is not spoken in this build"),
                    format!("option {other}"),
                ));
            }
            None => break,
        }
        if parser.comma().is_err() {
            break;
        }
    }
    if parens {
        parser.close_paren()?;
    }
    Ok(options)
}

/// A consumed option word that keyword() then declined — the peek and the
/// consume disagree only on a malformed statement.
fn format_err() -> Refused {
    Refused::surface(code::SYNTAX, "a COPY option was expected")
}

/// The text inside the query form's parentheses, quote-aware so a `)` in a
/// literal does not end the statement early.
fn parenthesised_query(parser: &mut Parser<'_>) -> Result<String, Refused> {
    let bytes = &parser.source[parser.at..];
    let mut depth = 1_usize;
    let mut at = 0_usize;
    while let Some(byte) = bytes.get(at) {
        match byte {
            b'(' => depth = depth.saturating_add(1),
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            b'\'' => {
                at = at.saturating_add(1);
                while let Some(quoted) = bytes.get(at) {
                    if *quoted == b'\'' {
                        // A doubled quote is the literal's own character.
                        if bytes.get(at.saturating_add(1)) == Some(&b'\'') {
                            at = at.saturating_add(2);
                            continue;
                        }
                        break;
                    }
                    at = at.saturating_add(1);
                }
            }
            _ => {}
        }
        at = at.saturating_add(1);
    }
    if depth != 0 {
        return Err(Refused::surface(
            code::SYNTAX,
            "the parenthesised query is not closed",
        ));
    }
    let inner = String::from_utf8_lossy(&bytes[..at])
        .trim()
        .trim_end_matches(';')
        .to_owned();
    parser.at = parser.at.saturating_add(at.saturating_add(1));
    Ok(inner)
}
