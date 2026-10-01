//! Declaring fields, analyzers and the geometry column.

use tessari_ql::{FieldPath, Name, Span, TableRef};
use tessari_storage::{
    Catalog, FieldShape, GEO_FIELD, IndexShape, TableKind, TableShape, Transaction,
};

use tessari_types::{Analyzer, FieldKind, Filter, IdentityKind, Path};

use crate::error::{Depended, Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// `DEFINE GEO places` — the collection, its geometry field and its index.
    ///
    /// The same three calls [`Session::define_vector`] makes, in the same order,
    /// through the same functions. There is no geo-only path: what the word
    /// creates is what the three statements create, which is what makes the
    /// round trip through `INFO` honest.
    pub(super) fn define_geo(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        if_not_exists: bool,
        span: Span,
    ) -> Result<Outcome> {
        let outcome = self.define_table(
            transaction,
            name,
            TableShape {
                schemafull: false,
                kind: TableKind::Geo,
                identity: IdentityKind::default(),
                graph: None,
                conflict: None,
                split: Vec::new(),
                partition: None,
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
            text: GEO_FIELD.to_owned(),
            span: name.span,
        };
        self.define_field(
            transaction,
            &field,
            &table,
            FieldKind::Geometry,
            FieldShape {
                // The property the three loose statements cannot express between
                // them, exactly as in the vector store: `TYPE geometry` leaves
                // the field optional, and a record with no geometry is legal in
                // a table while being a record a place store cannot answer for.
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
                path: Path::field(GEO_FIELD),
                span: name.span,
            }],
            IndexShape {
                unique: false,
                search: false,
                spatial: true,
                vector: None,
                costs: tessari_storage::SearchCosts::default(),
            },
            if_not_exists,
        )?;
        Ok(outcome)
    }

    pub(super) fn drop_geo(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let context = self.context(transaction, None, span)?;
        let id = Catalog::new(transaction)
            .table_id(context.namespace, context.database, &name.text)?
            .ok_or_else(|| Error::Unknown {
                entity: "geo store",
                name: name.text.clone(),
                span,
            })?;
        let is_geo = Catalog::new(transaction)
            .table(id)?
            .is_some_and(|definition| definition.kind == TableKind::Geo);
        if !is_geo {
            return Err(Error::Unknown {
                entity: "geo store",
                name: name.text.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_table(id)?;
        Ok(Outcome::Done)
    }

    pub(super) fn define_field(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        table: &TableRef,
        kind: FieldKind,
        shape: FieldShape,
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let (_, id) = self.resolve_table(transaction, table)?;
        if if_not_exists && self.field_named(transaction, id, name).is_ok() {
            return Ok(Outcome::Done);
        }
        if shape.secret
            && !Catalog::new(transaction)
                .table(id)?
                .is_some_and(|definition| definition.is_vault())
        {
            return Err(Error::SecretNeedsVault {
                table: table.name.text.clone(),
                span: table.span,
            });
        }
        // An analyzer is attached to a field by **name**, and nothing in the
        // catalog enforces the link — which is why `DROP ANALYZER` counts the
        // fields naming one before it removes it. Resolving the name here closes
        // that guard's other end. Without it a single misspelling reaches the
        // exact state the drop-side refusal exists to prevent, and the symptom
        // is not an error anybody sees: it is a search that quietly stops
        // matching.
        if let Some(named) = &shape.analyzer {
            let declared = Catalog::new(transaction)
                .analyzers()?
                .into_iter()
                .any(|held| &held.name == named);
            if !declared {
                return Err(Error::Unknown {
                    entity: "analyzer",
                    name: named.clone(),
                    span: name.span,
                });
            }
        }
        // The default is stored as the text it was written as, so it is read
        // back by parsing rather than by decoding a syntax tree — and a
        // definition stays legible in a dump.
        //
        // It is also **evaluated once, here**, and checked against the kind the
        // field declares. That is the same symmetry the rest of this wave
        // follows: a declaration is checked when it is made rather than when it
        // first bites. Without it, `DEFAULT 'open'` on a `TYPE int` field, or a
        // default naming something that does not exist, would be accepted and
        // would then fail on the first write — by which time the declaration is
        // in the catalog and the failure looks like the write's fault.
        if let Some(written) = &shape.default {
            let expression = tessari_ql::parse_expression(written)?;
            let value = self.evaluate(transaction, &expression)?;
            if !kind.accepts(&value) {
                return Err(Error::DefaultDoesNotMatch {
                    field: name.text.clone(),
                    declared: kind.name().into_owned(),
                    found: value.type_name(),
                    span: name.span,
                });
            }
        }
        Catalog::new(transaction).create_field(id, &name.text, kind, shape)?;
        Ok(Outcome::Done)
    }

    pub(super) fn define_analyzer(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        filters: &[Filter],
        if_not_exists: bool,
    ) -> Result<Outcome> {
        let declared = Catalog::new(transaction)
            .analyzers()?
            .into_iter()
            .any(|found| found.name == name.text);
        if if_not_exists && declared {
            return Ok(Outcome::Done);
        }
        Catalog::new(transaction).create_analyzer(&name.text, Analyzer::new(filters.to_vec()))?;
        Ok(Outcome::Done)
    }

    /// `DROP ANALYZER simple` — refused while a field still names it.
    ///
    /// The reference is by **name** rather than by id
    /// (`FieldDefinition::analyzer`), so nothing in the catalog enforces it and
    /// nothing would notice it break. What a dangling reference produces is a
    /// search that quietly stops matching — a wrong answer indistinguishable
    /// from a right one, which is the shape this store refuses everywhere.
    ///
    /// Every field in the store is read, not every field on one table: an
    /// analyzer is declared once for the whole store, so no single table can
    /// answer whether it is still attached.
    pub(super) fn drop_analyzer(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        span: Span,
    ) -> Result<Outcome> {
        let found = Catalog::new(transaction)
            .analyzers()?
            .into_iter()
            .find(|held| held.name == name.text);
        let Some(analyzer) = found else {
            return Err(Error::Unknown {
                entity: "analyzer",
                name: name.text.clone(),
                span,
            });
        };
        let attached: Vec<String> = Catalog::new(transaction)
            .fields()?
            .into_iter()
            .filter(|field| field.analyzer.as_deref() == Some(name.text.as_str()))
            .map(|field| field.name)
            .collect();
        if let Some(first) = attached.first() {
            return Err(Error::StillDepended {
                depended: Depended::AnalyzerByField,
                name: name.text.clone(),
                count: attached.len(),
                first: first.clone(),
                span,
            });
        }
        Catalog::new(transaction).drop_analyzer(analyzer.id)?;
        Ok(Outcome::Done)
    }
}
