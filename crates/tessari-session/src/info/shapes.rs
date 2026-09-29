//! How a table's declared shape, fields and indexes are described.

use std::collections::BTreeMap;
use tessari_storage::{FieldDefinition, IndexDefinition, TableDefinition};
use tessari_types::Value;

/// What a table declares about itself.
///
/// The three markers as the catalog holds them, rather than one word naming a
/// kind. A `DEFINE SPACE` and a plain `DEFINE TABLE` store the same markers, so
/// a report claiming to name the kind would be inventing a distinction the
/// catalog does not carry.
pub(crate) fn shape_of(definition: &TableDefinition) -> BTreeMap<String, Value> {
    let mut shape = BTreeMap::from([
        ("table".to_owned(), Value::from(definition.name.as_str())),
        ("schemafull".to_owned(), Value::Bool(definition.schemafull)),
        ("edge".to_owned(), Value::Bool(definition.is_edge())),
        ("bucket".to_owned(), Value::Bool(definition.is_bucket())),
        // Reported because it is **stored** and behaves like nothing else in the
        // report: a collection and a `SCHEMALESS` table accept the same writes,
        // so a report that omitted this would describe the two identically and a
        // declaration rebuilt from it would silently lose the word.
        (
            "collection".to_owned(),
            Value::Bool(definition.is_collection()),
        ),
        // Reported for the same reason and one stronger: a vault and a plain
        // table accept the same declarations to look at, so a report omitting
        // this describes them identically — and the one the report is about
        // refuses `SELECT`, seals its `SECRET` fields and cannot be made
        // schemaless. A declaration rebuilt from a report without it loses the
        // word `VAULT`, which is the word that mints the key.
        ("vault".to_owned(), Value::Bool(definition.is_vault())),
        // Reported for the same reason, and one more: it decides what the *next*
        // unnamed write is called, so a table read back without it looks like
        // every other table right up until a record is created under a scheme
        // nobody asked for.
        (
            "identity".to_owned(),
            Value::from(definition.identity.name()),
        ),
        // G027 S4.1, and reported for the reason `collection` and `vault` above
        // are, with the consequence one step further out. A table that declares
        // `LAST WRITER WINS` and one that declares nothing accept the same
        // writes and differ only in what happens to a write they cannot order:
        // the first takes it and counts the loss, the second refuses and names
        // both versions. Omit this and the two describe themselves identically,
        // and the declaration rebuilt below loses the words that decide which
        // one it is.
        //
        // `NONE` where nothing was declared, rather than an absent key: silence
        // is a refusal by decision (ADR-0075), not by default, and a report that
        // says nothing about it cannot be distinguished from one taken off a
        // build that had never heard of the clause.
        (
            "conflict".to_owned(),
            definition
                .conflict
                .map_or(Value::None, tessari_types::ConflictPolicy::to_value),
        ),
    ]);
    // Present only on a view, and it carries the read rather than a flag. A
    // marker alone would say the least useful true thing: two views differ
    // entirely in what they answer and not at all in being views, so a report
    // omitting the read describes every view identically. It is the same reason
    // the endpoint pair below is reported and not merely the edge flag.
    if let Some(read) = definition.view_read() {
        shape.insert("view".to_owned(), Value::from(read));
    }
    // Present only on a split table (G031, ADR-0080). Each bound is the literal
    // the clause takes — `'g'`, `uuid '…'` — so what the report prints is what
    // the next declaration types, and `NONE` marks an open end rather than a
    // shard with nothing in it.
    if let Some(shards) = &definition.shards {
        let bound = |at: Option<&tessari_types::RecordId>| {
            at.map_or(Value::None, |id| Value::from(id.to_literal().as_str()))
        };
        shape.insert(
            "shards".to_owned(),
            Value::Array(
                shards
                    .spans()
                    .map(|span| {
                        Value::Object(BTreeMap::from([
                            ("id".to_owned(), Value::from(i64::from(span.id.get()))),
                            ("from".to_owned(), bound(span.from)),
                            ("to".to_owned(), bound(span.to)),
                        ]))
                    })
                    .collect(),
            ),
        );
    }
    // Present only on a table that belongs to one, and reported as the **id**
    // for the reason the endpoints below are: this report says what is stored,
    // and a name resolved here would be a second read able to disagree with the
    // first. `INFO FOR GRAPH` is the reverse direction and takes the name.
    if let Some(graph) = definition.graph {
        shape.insert("graph".to_owned(), Value::from(i64::from(graph.get())));
    }
    // Present only on an edge table that declared its pair, and it has to be
    // present there: the endpoints and the order are the whole of what the
    // clause adds, and a declared pair reported as a bare edge table would be
    // described identically to one that accepts writes it refuses — the same
    // failure the `collection` marker above exists to prevent, one clause
    // further on.
    //
    // The endpoints are reported as **table ids**, because that is what the
    // catalog holds and this report says what is stored. Resolving them to names
    // would be a second read that can disagree with the first.
    if let Some(endpoints) = definition.edge_endpoints() {
        let mut declared = BTreeMap::from([
            (
                "from".to_owned(),
                Value::from(i64::from(endpoints.from.get())),
            ),
            ("to".to_owned(), Value::from(i64::from(endpoints.to.get()))),
        ]);
        if let Some(order) = &endpoints.order {
            declared.insert("order".to_owned(), Value::from(order.field.as_str()));
            declared.insert("descending".to_owned(), Value::Bool(order.descending));
        }
        shape.insert("endpoints".to_owned(), Value::Object(declared));
    }
    shape
}

/// One declared field.
pub(crate) fn described_field(field: &FieldDefinition) -> Value {
    let mut described = BTreeMap::from([
        ("name".to_owned(), Value::from(field.name.as_str())),
        ("type".to_owned(), Value::from(field.kind.name().as_ref())),
        ("required".to_owned(), Value::Bool(field.required)),
    ]);
    if let Some(default) = &field.default {
        described.insert("default".to_owned(), Value::from(default.as_str()));
    }
    if let Some(analyzer) = &field.analyzer {
        described.insert("analyzer".to_owned(), Value::from(analyzer.as_str()));
    }
    if let Some(assert) = &field.assert {
        // The stored constraint, not a sentence describing it — the catalog
        // holds the lowered form and this is it.
        described.insert("assert".to_owned(), assert.to_value());
    }
    Value::Object(described)
}

/// One declared index.
pub(crate) fn described_index(index: &IndexDefinition) -> Value {
    let mut described = BTreeMap::from([
        ("name".to_owned(), Value::from(index.name.as_str())),
        (
            "fields".to_owned(),
            Value::Array(
                index
                    .fields
                    .iter()
                    .map(|path| Value::String(path.to_string()))
                    .collect(),
            ),
        ),
        ("unique".to_owned(), Value::Bool(index.unique)),
        ("search".to_owned(), Value::Bool(index.search)),
        // The fourth kind. It was missing here while the catalog has carried it
        // all along, so a spatial index read back as an ordinary one — a report
        // that said the index answers ranges when it answers cells.
        ("spatial".to_owned(), Value::Bool(index.spatial)),
    ]);
    if let Some(distance) = index.vector {
        described.insert("vector".to_owned(), Value::from(distance.name()));
    }
    Value::Object(described)
}
