//! Declaring, dropping and describing a search and its word sets (ADR-0105).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use tessari_ql::{Name, SearchMember, Span};
use tessari_storage::{
    Catalog, EngineField, EngineMember, IndexDefinition, Transaction, UNIT_WEIGHT, WordSet,
    WordSetKind,
};
use tessari_types::{DatabaseId, NamespaceId, Number, Value};

use crate::error::{Depended, Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

/// The largest weight a field may be given, in thousandths: a thousand.
const WEIGHT_CEILING: i64 = 1_000_000;

/// Refuse to remove a store-wide name some search still reads.
///
/// # Errors
///
/// [`Error::StillDepended`] naming the first search found.
pub(crate) fn refuse_named_by_a_search(
    transaction: &mut Transaction<'_>,
    depended: Depended,
    name: &str,
    span: Span,
    names: impl Fn(&EngineMember) -> bool,
) -> Result<()> {
    let searches: BTreeSet<String> = Catalog::new(transaction)
        .engine_members()?
        .into_iter()
        .filter_map(|member| member.engine)
        .filter(|engine| names(engine))
        .map(|engine| engine.search)
        .collect();
    if let Some(first) = searches.first() {
        return Err(Error::StillDepended {
            depended,
            name: name.to_owned(),
            count: searches.len(),
            first: first.clone(),
            span,
        });
    }
    Ok(())
}

impl Session<'_> {
    /// `DEFINE SEARCH`: one member index per table, written in this
    /// transaction and built from the table's records in the same batch.
    pub(crate) fn define_search(
        &self,
        transaction: &mut Transaction<'_>,
        (name, members, analyzer, stopwords): (&Name, &[SearchMember], &Name, Option<&Name>),
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        if !Catalog::new(transaction)
            .members_of(context.namespace, context.database, &name.text)?
            .is_empty()
        {
            if if_not_exists {
                return Ok(Outcome::Done);
            }
            return Err(Error::SearchExists {
                name: name.text.clone(),
                span: name.span,
            });
        }
        if !Catalog::new(transaction)
            .analyzers()?
            .iter()
            .any(|held| held.name == analyzer.text)
        {
            return Err(Error::Unknown {
                entity: "analyzer",
                name: analyzer.text.clone(),
                span: analyzer.span,
            });
        }
        if let Some(set) = stopwords {
            self.known_set(transaction, WordSetKind::Stopwords, set)?;
        }
        let mut tables = BTreeSet::new();
        let mut planned = Vec::with_capacity(members.len());
        for member in members {
            let (_, table) = self.resolve_table(transaction, &member.table)?;
            if !tables.insert(table) {
                return Err(Error::SearchNamesTableTwice {
                    table: member.table.name.text.clone(),
                    span: member.table.span,
                });
            }
            let Some(definition) = Catalog::new(transaction).table(table)? else {
                continue;
            };
            if definition.is_vault() {
                return Err(Error::NotReadBySelect {
                    table: member.table.name.text.clone(),
                    span: member.table.span,
                });
            }
            let mut paths = Vec::with_capacity(member.fields.len());
            let mut fields = Vec::with_capacity(member.fields.len());
            for field in &member.fields {
                if paths.contains(&field.path.path) {
                    return Err(Error::SearchNamesFieldTwice {
                        field: field.path.path.to_string(),
                        span: field.path.span,
                    });
                }
                if let Some(set) = &field.synonyms {
                    self.known_set(transaction, WordSetKind::Synonyms, set)?;
                }
                paths.push(field.path.path.clone());
                fields.push(EngineField {
                    weight: weight_of(field.weight.as_ref(), field.path.span)?,
                    fuzzy: field.fuzzy,
                    prefix: field.prefix,
                    phrase: field.phrase,
                    synonyms: field.synonyms.as_ref().map(|set| set.text.clone()),
                    snippet: field.snippet,
                });
            }
            planned.push((table, paths, fields));
        }
        for (table, paths, fields) in planned {
            Catalog::new(transaction).create_member(
                table,
                paths,
                EngineMember {
                    search: name.text.clone(),
                    analyzer: analyzer.text.clone(),
                    stopwords: stopwords.map(|set| set.text.clone()),
                    fields,
                },
            )?;
        }
        Ok(Outcome::Done)
    }

    /// `DROP SEARCH`: every member goes, and the index path sweeps its entries.
    pub(crate) fn drop_search(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let members = Catalog::new(transaction).members_of(
            context.namespace,
            context.database,
            &name.text,
        )?;
        if members.is_empty() {
            return Err(Error::Unknown {
                entity: "search",
                name: name.text.clone(),
                span: name.span,
            });
        }
        for member in members {
            Catalog::new(transaction).drop_index(member.id)?;
        }
        Ok(Outcome::Done)
    }

    /// `DEFINE SYNONYMS` and `DEFINE STOPWORDS`.
    pub(crate) fn define_word_set(
        &self,
        transaction: &mut Transaction<'_>,
        kind: WordSetKind,
        name: &Name,
        entries: BTreeMap<String, Vec<String>>,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        if Catalog::new(transaction)
            .word_set(kind, &name.text)?
            .is_some()
            && if_not_exists
        {
            return Ok(Outcome::Done);
        }
        // One word per entry and per alternative: the position policy for a
        // synonym is that it stands in one position, so a phrase across it is
        // the same length either way (ADR-0105 D5).
        for word in entries.keys().chain(entries.values().flatten()) {
            let pieces = word
                .split(|character: char| !character.is_alphanumeric())
                .filter(|piece| !piece.is_empty())
                .count();
            if pieces != 1 {
                return Err(Error::NotOneWord {
                    word: word.clone(),
                    span: name.span,
                });
            }
        }
        Catalog::new(transaction).create_word_set(&WordSet {
            kind,
            name: name.text.clone(),
            entries,
        })?;
        Ok(Outcome::Done)
    }

    /// `DROP SYNONYMS` and `DROP STOPWORDS`, refused while a search names it.
    pub(crate) fn drop_word_set(
        &self,
        transaction: &mut Transaction<'_>,
        kind: WordSetKind,
        name: &Name,
    ) -> Result<Outcome> {
        let (depended, named): (Depended, fn(&EngineMember, &str) -> bool) = match kind {
            WordSetKind::Synonyms => (Depended::SynonymsBySearch, |engine, set| {
                engine
                    .fields
                    .iter()
                    .any(|field| field.synonyms.as_deref() == Some(set))
            }),
            WordSetKind::Stopwords => (Depended::StopwordsBySearch, |engine, set| {
                engine.stopwords.as_deref() == Some(set)
            }),
        };
        refuse_named_by_a_search(transaction, depended, &name.text, name.span, |engine| {
            named(engine, &name.text)
        })?;
        if !Catalog::new(transaction).drop_word_set(kind, &name.text)? {
            return Err(Error::Unknown {
                entity: kind.word(),
                name: name.text.clone(),
                span: name.span,
            });
        }
        Ok(Outcome::Done)
    }

    fn known_set(
        &self,
        transaction: &mut Transaction<'_>,
        kind: WordSetKind,
        set: &Name,
    ) -> Result<()> {
        if Catalog::new(transaction)
            .word_set(kind, &set.text)?
            .is_none()
        {
            return Err(Error::Unknown {
                entity: kind.word(),
                name: set.text.clone(),
                span: set.span,
            });
        }
        Ok(())
    }

    /// `INFO FOR SEARCH`: the analyzer, the stop words and, per member, its
    /// table, fields and documents.
    pub(crate) fn info_search(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<BTreeMap<String, Value>> {
        let context = self.context(transaction, None, span)?;
        let mut members = Catalog::new(transaction).members_of(
            context.namespace,
            context.database,
            &name.text,
        )?;
        let Some(engine) = members.first().and_then(|member| member.engine.clone()) else {
            return Err(Error::Unknown {
                entity: "search",
                name: name.text.clone(),
                span: name.span,
            });
        };
        let mut described = Vec::with_capacity(members.len());
        let mut tables = BTreeMap::new();
        for member in &members {
            if let Some(table) = Catalog::new(transaction).table(member.table)? {
                tables.insert(member.id, table.name);
            }
        }
        members.sort_by_key(|member| tables.get(&member.id).cloned());
        for member in &members {
            let (statistics, _) = transaction.member_statistics(member)?;
            let fields = member
                .fields
                .iter()
                .zip(member.engine.iter().flat_map(|engine| &engine.fields))
                .map(|(path, field)| {
                    let mut described = BTreeMap::from([
                        ("field".to_owned(), Value::from(path.to_string().as_str())),
                        (
                            "weight".to_owned(),
                            Value::Number(weight_value(field.weight)),
                        ),
                        ("fuzzy".to_owned(), Value::Bool(field.fuzzy)),
                        ("prefix".to_owned(), Value::Bool(field.prefix)),
                        ("phrase".to_owned(), Value::Bool(field.phrase)),
                        ("snippet".to_owned(), Value::Bool(field.snippet)),
                    ]);
                    if let Some(set) = &field.synonyms {
                        described.insert("synonyms".to_owned(), Value::from(set.as_str()));
                    }
                    Value::Object(described)
                })
                .collect();
            described.push(Value::Object(BTreeMap::from([
                (
                    "table".to_owned(),
                    Value::from(tables.get(&member.id).map_or("", String::as_str)),
                ),
                ("fields".to_owned(), Value::Array(fields)),
                (
                    "documents".to_owned(),
                    Value::Number(Number::Integer(
                        i64::try_from(statistics.documents).unwrap_or(i64::MAX),
                    )),
                ),
            ])));
            if let Some(Value::Object(last)) = described.last_mut() {
                last.extend(crate::info::tokenizer_report(member));
            }
        }
        let mut info = BTreeMap::from([
            ("name".to_owned(), Value::from(name.text.as_str())),
            ("analyzer".to_owned(), Value::from(engine.analyzer.as_str())),
            ("members".to_owned(), Value::Array(described)),
        ]);
        if let Some(set) = &engine.stopwords {
            info.insert("stopwords".to_owned(), Value::from(set.as_str()));
        }
        Ok(info)
    }
}

/// A weight as written, in thousandths; absent is one.
fn weight_of(written: Option<&Number>, span: Span) -> Result<u32> {
    let Some(written) = written else {
        return Ok(UNIT_WEIGHT);
    };
    let thousandths = written
        .as_float()
        .filter(|weight| weight.is_finite())
        .and_then(|weight| Number::float((weight * 1000.0).round()).as_exact_integer())
        .filter(|thousandths| (1..=WEIGHT_CEILING).contains(thousandths))
        .and_then(|thousandths| u32::try_from(thousandths).ok());
    thousandths.ok_or(Error::WeightOutOfRange { span })
}

/// A stored weight as the number a reader wrote.
fn weight_value(thousandths: u32) -> Number {
    if thousandths.is_multiple_of(UNIT_WEIGHT) {
        Number::Integer(i64::from(thousandths / UNIT_WEIGHT))
    } else {
        Number::float(f64::from(thousandths) / f64::from(UNIT_WEIGHT))
    }
}

/// A stored weight as the literal a script writes back.
fn weight_text(thousandths: u32) -> String {
    let whole = thousandths / UNIT_WEIGHT;
    let part = thousandths % UNIT_WEIGHT;
    if part == 0 {
        return whole.to_string();
    }
    let fraction = format!("{part:03}");
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

/// Every word set, once, as the statements that declare them — `IF NOT EXISTS`
/// in a part of the store, whose target may hold its own of that name.
pub(crate) fn word_sets_script(catalog: &Catalog<'_, '_>, part: bool) -> Result<String> {
    let mut written = String::new();
    let guard = if part { "IF NOT EXISTS " } else { "" };
    for set in catalog.word_sets()? {
        let quoted = |word: &str| format!("'{}'", word.replace('\\', "\\\\").replace('\'', "\\'"));
        let body = match set.kind {
            WordSetKind::Synonyms => format!(
                "{{ {} }}",
                set.entries
                    .iter()
                    .map(|(word, alternatives)| format!(
                        "{}: [{}]",
                        quoted(word),
                        alternatives
                            .iter()
                            .map(|one| quoted(one))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            WordSetKind::Stopwords => format!(
                "[{}]",
                set.entries
                    .keys()
                    .map(|word| quoted(word))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let _ = writeln!(
            written,
            "DEFINE {} {guard}{} {body};",
            set.kind.word().to_uppercase(),
            set.name
        );
    }
    Ok(written)
}

/// The `DEFINE SEARCH` statements of one database, members in table order.
pub(crate) fn searches_script(
    catalog: &Catalog<'_, '_>,
    namespace: NamespaceId,
    database: DatabaseId,
) -> Result<String> {
    let mut by_search: BTreeMap<String, Vec<(String, IndexDefinition)>> = BTreeMap::new();
    for member in catalog.engine_members()? {
        if member.namespace != namespace || member.database != database {
            continue;
        }
        let Some(engine) = &member.engine else {
            continue;
        };
        let Some(table) = catalog.table(member.table)? else {
            continue;
        };
        by_search
            .entry(engine.search.clone())
            .or_default()
            .push((table.name, member));
    }
    let mut written = String::new();
    for (name, mut members) in by_search {
        members.sort_by(|left, right| left.0.cmp(&right.0));
        let Some(engine) = members
            .first()
            .and_then(|(_, member)| member.engine.clone())
        else {
            continue;
        };
        let _ = write!(written, "DEFINE SEARCH {name}");
        for (table, member) in &members {
            let fields = member
                .fields
                .iter()
                .zip(member.engine.iter().flat_map(|engine| &engine.fields))
                .map(|(path, field)| {
                    let mut text = path.to_string();
                    if field.weight != UNIT_WEIGHT {
                        let _ = write!(text, " WEIGHT {}", weight_text(field.weight));
                    }
                    for (allowed, word) in [
                        (field.fuzzy, " NO FUZZY"),
                        (field.prefix, " NO PREFIX"),
                        (field.phrase, " NO PHRASE"),
                    ] {
                        if !allowed {
                            text.push_str(word);
                        }
                    }
                    if let Some(set) = &field.synonyms {
                        let _ = write!(text, " SYNONYMS {set}");
                    }
                    if field.snippet {
                        text.push_str(" SNIPPET");
                    }
                    text
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = write!(written, " ON {table} FIELDS {fields}");
        }
        let _ = write!(written, " ANALYZER {}", engine.analyzer);
        if let Some(set) = &engine.stopwords {
            let _ = write!(written, " STOPWORDS {set}");
        }
        written.push_str(";\n");
    }
    Ok(written)
}
