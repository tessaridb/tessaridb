//! Declaring tables, indexes and vectors, and dropping the kinds of table.

use tessari_ql::{ColumnDeclaration, FieldPath, Name, Span, TableRef};
use tessari_storage::{
    Catalog, FieldShape, IndexShape, TableKind, TableShape, Transaction, VECTOR_FIELD,
    VectorDeclaration, VectorDistance,
};

use tessari_types::{FieldKind, IdentityKind, Path};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

/// What `DEFINE VECTOR` declares about the store, beside its name.
pub(super) struct VectorStore<'a> {
    /// How wide every vector is.
    pub(super) dimension: usize,
    /// The distance its index is built with, as written.
    pub(super) distance: &'a Name,
    /// Whether its index keeps vectors as one byte per component.
    pub(super) quantized: bool,
}

impl Session<'_> {
    pub(crate) fn define_table(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        shape: TableShape,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        if if_not_exists
            && Catalog::new(transaction)
                .table_id(context.namespace, context.database, &name.text)?
                .is_some()
        {
            return Ok(Outcome::Done);
        }
        Catalog::new(transaction).create_table(
            context.namespace,
            context.database,
            &name.text,
            shape,
        )?;
        Ok(Outcome::Done)
    }

    /// The definition is written here; its entries are built by the commit.
    ///
    /// Nothing more is needed at this layer, and that is the point: a catalog
    /// entry is an ordinary record, so index maintenance sees the definition in
    /// the same log record and projects the table's rows under it into the same
    /// batch. The definition and its entries land together or neither does,
    /// inside an open `BEGIN` as much as outside one.
    pub(super) fn define_index(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        table: &TableRef,
        fields: &[FieldPath],
        shape: IndexShape,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let (_, id) = self.resolve_table(transaction, table)?;
        if if_not_exists && self.index_named(transaction, id, name).is_ok() {
            return Ok(Outcome::Done);
        }
        self.refuse_indexing_a_secret(transaction, id, table, fields)?;
        let fields = fields.iter().map(|field| field.path.clone()).collect();
        Catalog::new(transaction).create_index(id, &name.text, fields, shape)?;
        Ok(Outcome::Done)
    }

    /// The declaration is written here; the rows answer for it in the commit.
    ///
    /// Nothing more is needed at this layer, for the reason `define_index` needs
    /// nothing more: a catalog entry is an ordinary record, so the store's schema
    /// check sees the declaration in the same log record and holds every row of
    /// the table to it — the ones already there and the ones this transaction
    /// writes. The declaration and the rows it constrains land together or
    /// neither does.
    /// `DEFINE TABLE t (a string, b int REQUIRED)` — the table, then its fields.
    ///
    /// **A desugaring, not a second implementation.** Each column goes through
    /// the same `define_field` the long spelling does, which is what makes a
    /// constraint declared here behave the way one declared there does — it is
    /// checked against the rows already in the table, and a violation refuses
    /// the whole statement. Reimplementing the field half would have produced
    /// the one thing this criterion is about: two spellings that agree on the
    /// happy path and disagree on the day the data does not fit.
    ///
    /// Fields are declared **after** the table exists and in written order, so
    /// a failure at the third column rolls back the first two and the table
    /// with them: the statement's transaction is the unit, and a half-declared
    /// table is not a state this can leave behind.
    ///
    /// `IF NOT EXISTS` covers the whole declaration rather than the table
    /// alone. The alternative makes the statement un-re-runnable — the table is
    /// tolerated and the first column then refuses — which is the opposite of
    /// what the words ask for.
    pub(super) fn define_table_with_columns(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        columns: &[ColumnDeclaration],
        shape: TableShape,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let outcome = self.define_table(transaction, name, shape, if_not_exists, span)?;
        if columns.is_empty() {
            return Ok(outcome);
        }
        let table = TableRef {
            database: None,
            name: name.clone(),
            span: name.span,
        };
        for column in columns {
            self.define_field(
                transaction,
                &column.name,
                &table,
                column.kind.clone(),
                FieldShape {
                    required: column.required,
                    secret: false,
                    default: column.default.as_ref().map(|written| written.text.clone()),
                    analyzer: column.analyzer.as_ref().map(|named| named.text.clone()),
                    assert: column.assert.clone(),
                },
                if_not_exists,
            )?;
        }
        Ok(outcome)
    }

    /// `DEFINE VECTOR embeddings DIMENSION 768 DISTANCE cosine`
    ///
    /// **A desugaring, not a second implementation**, on exactly the reasoning
    /// [`Self::define_table_with_columns`] records. The statement stands for
    /// three:
    ///
    /// ```text
    /// DEFINE COLLECTION embeddings;
    /// DEFINE FIELD vector ON embeddings TYPE vector<768> REQUIRED;
    /// DEFINE INDEX vector ON embeddings FIELDS vector VECTOR cosine;
    /// ```
    ///
    /// and it runs them through the same three functions the long spellings do.
    /// That is what discharges the criterion this node was written for: there is
    /// no store-only path that could disagree with the field one, because the
    /// store's path **is** the field one. A width declared here is checked by
    /// the store's apply pass, where a replica reaches the same verdict from the
    /// record alone — not because this function arranged it, but because there
    /// is nothing else here to arrange.
    ///
    /// The field and the index share the name `vector`. Fields and indexes are
    /// separate namespaces, so nothing collides, and the store then has one name
    /// to remember rather than two — `INFO FOR TABLE` reads
    /// `DEFINE INDEX vector ON embeddings FIELDS vector VECTOR cosine`, which
    /// says what it is.
    ///
    /// `IF NOT EXISTS` covers all three, for the reason it covers a table and
    /// its columns: tolerating the table and then refusing at the field makes
    /// the statement un-re-runnable, which is the opposite of what the words ask.
    pub(super) fn define_vector(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        store: VectorStore<'_>,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let VectorStore {
            dimension,
            distance,
            quantized,
        } = store;
        let distance =
            VectorDistance::parse(&distance.text).ok_or_else(|| Error::NoSuchDistance {
                name: distance.text.clone(),
                span: distance.span,
            })?;
        // Written as a conversion rather than a cast, so that raising the
        // ceiling is a question the compiler asks rather than a truncation
        // nobody sees. The parser refuses anything above it, so the only way
        // here is through a change to that ceiling.
        let held = u32::try_from(dimension).map_err(|_| {
            tessari_ql::Error::VectorWidthAboveTheCeiling {
                most: tessari_ql::WIDEST_VECTOR,
                span,
            }
        })?;
        let outcome = self.define_table(
            transaction,
            name,
            TableShape {
                schemafull: false,
                kind: TableKind::Vector(VectorDeclaration {
                    dimension: held,
                    distance,
                }),
                identity: IdentityKind::default(),
                graph: None,
                conflict: None,
                split: Vec::new(),
                partition: None,
                spread: false,
            },
            if_not_exists,
            span,
        )?;
        let table = TableRef {
            database: None,
            name: name.clone(),
            span: name.span,
        };
        let field = Name {
            text: VECTOR_FIELD.to_owned(),
            span: name.span,
        };
        self.define_field(
            transaction,
            &field,
            &table,
            FieldKind::vector(dimension).ok_or(tessari_ql::Error::VectorWidthBelowOne { span })?,
            FieldShape {
                // The one property the three loose statements cannot express
                // between them: `TYPE vector<n>` leaves a field optional, so a
                // record with no vector at all is legal in a table — and is not
                // a record of a vector store.
                required: true,
                secret: false,
                default: None,
                analyzer: None,
                assert: None,
            },
            if_not_exists,
        )?;
        self.define_index(
            transaction,
            &field,
            &table,
            &[FieldPath {
                path: Path::field(VECTOR_FIELD),
                span: name.span,
            }],
            IndexShape {
                unique: false,
                search: false,
                spatial: false,
                quantized,
                vector: Some(distance),
                costs: tessari_storage::SearchCosts::default(),
            },
            if_not_exists,
        )?;
        Ok(outcome)
    }

    /// `DROP VECTOR embeddings` — the store's definition, its field and its
    /// index declaration.
    ///
    /// **Not its records.** This calls [`Catalog::drop_table`], which removes the
    /// catalog rows and does not reach what is stored under them — the rule every
    /// drop in this language follows, and the reason there is no `CASCADE`. The
    /// summary line said "its records" until it was read against the code; a
    /// wrong sentence here is worse than a wrong one in the manual, because the
    /// next person to reason about the statement reads it and stops checking.
    ///
    /// Refuses a table that is not one, rather than dropping it. The two words
    /// name different things even where they would remove the same rows, and a
    /// `DROP VECTOR` that quietly removed an ordinary table would be a typo with
    /// the blast radius of a table.
    /// `DROP QUEUE jobs`
    ///
    /// Refuses a table that is not a queue by reporting it as unknown, the shape
    /// `DROP VECTOR` already uses: a word that removed a table of another kind
    /// would make `DROP QUEUE` a second spelling of `DROP TABLE`, and the two
    /// answer to different grants for different reasons.
    /// `DROP SERIES readings`
    ///
    /// Refuses a name that is not a series for [`Self::drop_queue`]'s reason:
    /// the word in the statement is a claim about what is being removed, and a
    /// `DROP SERIES` that removed a plain table would be a statement doing
    /// something other than what it says.
    pub(super) fn drop_series(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let unknown = || Error::Unknown {
            entity: "series",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(unknown)?;
        let series = match Catalog::new(transaction).table(id)?.map(|found| found.kind) {
            Some(TableKind::Series(series)) => series,
            _ => return Err(unknown()),
        };
        if !series.rollups.is_empty() {
            return Err(Error::RollupsDependOn {
                series: name.text.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
    }

    pub(super) fn drop_queue(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let unknown = || Error::Unknown {
            entity: "queue",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(unknown)?;
        let is_queue = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| matches!(definition.kind, TableKind::Queue(_)));
        if !is_queue {
            return Err(unknown());
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
    }

    /// `DROP VIEW active` — the definition, and there is nothing else.
    ///
    /// A view holds no records, no index and no keyspace, so dropping one frees
    /// nothing and orphans nothing. It refuses a name of another kind for the
    /// reason `DROP QUEUE` does: a word that removed a table of another kind
    /// would make the statement's own name the least reliable thing about it.
    pub(super) fn drop_view(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let unknown = || Error::Unknown {
            entity: "view",
            name: name.text.clone(),
            span,
        };
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(unknown)?;
        let kind = Catalog::new(transaction)
            .table(id)?
            .map(|definition| definition.kind);
        let Some(TableKind::View(declared)) = kind else {
            return Err(unknown());
        };
        // A kept view's rows go with its table; its state is kept beside them
        // and goes too (ADR-0109 D1).
        if declared.materialized {
            transaction.forget_view(id);
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
    }

    pub(super) fn drop_vector(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "vector store",
                name: name.text.clone(),
                span,
            })?;
        let is_vector = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| matches!(definition.kind, TableKind::Vector(_)));
        if !is_vector {
            return Err(Error::Unknown {
                entity: "vector store",
                name: name.text.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
    }
}
