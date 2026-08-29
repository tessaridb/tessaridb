//! Reading the specification as data.
//!
//! `docs/tessariql.md` is prose with executable examples in it, and two
//! different checks need to take it apart: one parses every example to catch the
//! document and the parser drifting, the other asks whether the document names
//! every kind, function and statement form the engine actually has.
//!
//! Both need the same two operations — find the fenced blocks, cut them into
//! statements — so they live here rather than twice in `tests/`, where the
//! second copy would be the one that quietly stopped matching the first.

use std::fs;
use std::path::{Path, PathBuf};

/// Where the specification lives, relative to this crate.
const SPECIFICATION: &str = "../../docs/tessariql.md";

/// The path of the specification, as an absolute path.
///
/// # Panics
///
/// When the document is missing. It is checked in beside the code, so its
/// absence is a broken checkout rather than a condition to handle.
#[must_use]
pub fn specification_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(SPECIFICATION)
        .canonicalize()
        .expect("docs/tessariql.md is missing")
}

/// The text of the specification.
///
/// # Panics
///
/// When the document is missing or unreadable.
#[must_use]
pub fn specification() -> String {
    fs::read_to_string(specification_path()).expect("docs/tessariql.md is unreadable")
}

/// The label a fence may carry and still hold TessariQL.
///
/// Most example blocks are unmarked, but a few are marked with the language.
/// Both are scripts, and skipping the marked ones let four statements sit in
/// the document unchecked — including the only example of `ALTER FIELD`, which
/// then read as undocumented to the ratchet while being right there on the
/// page.
const TESSARIQL: &str = "tessariql";

/// The fenced blocks of a markdown file, with the line each began on.
///
/// A block is TessariQL when its fence is unmarked or marked [`TESSARIQL`];
/// any other label is skipped, because a `json` block is a response and a
/// `text` block is a diagram.
#[must_use]
pub fn fenced_blocks(text: &str) -> Vec<(usize, String)> {
    let mut blocks = Vec::new();
    // Whether a fence is open, and whether what it holds is TessariQL. A
    // **labelled** fence (```json, ```text) is tracked as open even though its
    // body is skipped: without that its closing line reads as an opener, every
    // fence after it pairs with the wrong partner, and the document's prose
    // starts arriving here as though it were a statement. That inverted
    // silently until a labelled fence was added beside a bare one.
    let mut open: Option<(usize, String, bool)> = None;
    for (index, line) in text.lines().enumerate() {
        let number = index.saturating_add(1);
        if let Some(rest) = line.trim_end().strip_prefix("```") {
            match open.take() {
                Some((at, body, checked)) => {
                    if checked {
                        blocks.push((at, body));
                    }
                }
                None => {
                    let label = rest.trim();
                    open = Some((
                        number.saturating_add(1),
                        String::new(),
                        label.is_empty() || label == TESSARIQL,
                    ));
                }
            }
            continue;
        }
        if let Some((_, body, _)) = open.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    blocks
}

/// The statements of a block, kept whole.
///
/// Splitting on `;` alone would cut a string containing one in half, so the
/// scan tracks quoting the way the lexer does.
#[must_use]
pub fn split_statements(block: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for character in block.chars() {
        current.push(character);
        match quote {
            Some(open) => {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == open {
                    quote = None;
                }
            }
            None => match character {
                '\'' | '"' => quote = Some(character),
                ';' => {
                    statements.push(core::mem::take(&mut current));
                }
                _ => {}
            },
        }
    }
    if !current.trim().is_empty() {
        statements.push(current);
    }
    statements
        .into_iter()
        .filter(|statement| !statement.trim().is_empty())
        .collect()
}

/// Every statement of every unlabelled fenced block, with its line.
#[must_use]
pub fn examples(text: &str) -> Vec<(usize, String)> {
    fenced_blocks(text)
        .into_iter()
        .flat_map(|(number, block)| {
            split_statements(&block)
                .into_iter()
                .map(move |statement| (number, statement))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{examples, split_statements};

    #[test]
    fn a_string_holding_a_semicolon_is_not_cut_in_half() {
        let split = split_statements("SET k:1 = 'a;b'; SELECT * FROM users;");
        assert_eq!(split.len(), 2, "{split:?}");
        assert!(split[0].contains("'a;b'"), "{split:?}");
    }

    #[test]
    fn a_foreign_fence_is_skipped_without_unpairing_the_next_one() {
        let text = "```json\n{\"a\": 1}\n```\n\n```\nSELECT * FROM users;\n```\n";
        let found = examples(text);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].1.contains("SELECT"), "{found:?}");
    }

    #[test]
    fn a_fence_labelled_with_the_language_is_read_like_a_bare_one() {
        let text = "```tessariql\nDROP TABLE notes;\n```\n";
        let found = examples(text);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].1.contains("DROP TABLE"), "{found:?}");
    }
}
