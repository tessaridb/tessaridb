//! A bounded order by distance from a position, served by a spatial index —
//! over points or areas, with or without a condition (G058 C1).
//!
//! The walk ([`tessari_storage::PlacesNearest`]) hands records out on floors
//! that nothing in them can beat; this side fetches each one, tests the whole
//! condition, and measures it by the statement's **own** ordering expression,
//! so the order and every distance are the scan's to the last digit. It stops
//! once it holds as many as the read wants and the next floor is beyond the
//! worst of them — the records it never took are records it has shown cannot
//! rank above the ones it holds.
//!
//! The ordering stage still sorts and bounds what comes back, exactly as it
//! sorts a scan's: the records at the last distance travel together, so the
//! tie group is the scan's too.

use tessari_constants::SPATIAL_NEAREST_EXAMINATION_CAP;
use tessari_ql::Expr;
use tessari_storage::{RecordAddress, Transaction};
use tessari_types::{RecordId, TableId, Value};

use crate::condition::boolean;
use crate::error::Result;
use crate::plan;
use crate::session::Session;

use super::{Scope, Walked};

impl Session<'_> {
    /// Walk a spatial index nearest-first, when there is one that answers this
    /// read.
    ///
    /// `NotServed` is no index for it: none on the field, the field hidden from
    /// this caller, this transaction's own writes or a snapshot behind the
    /// committed tail (all in [`Self::index_serving_place`]), or a query that is
    /// not a position. `Declined` is an index that could not settle it: the walk
    /// ran out before it held enough — the records without a shape belong to the
    /// answer then, and the index holds none of them — a distance that does not
    /// converge, or more records taken than [`SPATIAL_NEAREST_EXAMINATION_CAP`].
    pub(super) fn walk_to_place(
        &self,
        transaction: &mut Transaction<'_>,
        context: crate::context::Context,
        table: TableId,
        wanted: &plan::Closest<'_>,
        condition: Option<&Expr>,
        scope: Scope<'_>,
    ) -> Result<Walked> {
        let Some((index, visible)) =
            self.index_serving_place(transaction, context, table, wanted.path)?
        else {
            return Ok(Walked::NotServed);
        };
        let Value::Geometry(tessari_types::Geometry::Point(position)) =
            self.evaluate(transaction, wanted.query)?
        else {
            return Ok(Walked::NotServed);
        };
        let Ok(target) = tessari_geo::Snapped::of(position) else {
            return Ok(Walked::NotServed);
        };
        if wanted.wanted == 0 {
            return Ok(Walked::Served {
                found: Vec::new(),
                index: index.name,
            });
        }
        // A record near the antipode measures `NONE`, which sorts first, while
        // its floor is half the planet: the walk would reach it last. Anything
        // the index holds there gives the read to the scan.
        for zone in tessari_geo::antipodal_zone(target) {
            let cells: Vec<tessari_geo::Cell> =
                tessari_geo::covering(zone, tessari_constants::SPATIAL_QUERY_CELLS)
                    .into_iter()
                    .map(|(cell, _)| cell)
                    .collect();
            let near = transaction.records_in_region(
                &index,
                &cells,
                zone,
                tessari_geo::Relation::Meets,
            )?;
            if !near.rows.is_empty() {
                return Ok(Walked::Declined);
            }
        }
        let mut walk = transaction.places_nearest(&index, target);
        let mut held: Vec<(f64, RecordId, Value)> = Vec::new();
        let mut taken = 0_usize;
        loop {
            let beyond = if held.len() >= wanted.wanted {
                held.last().map_or(f64::INFINITY, |(metres, _, _)| *metres)
            } else {
                f64::INFINITY
            };
            let Some((_, id)) = walk.next(transaction, beyond)? else {
                break;
            };
            taken = taken.saturating_add(1);
            if taken > SPATIAL_NEAREST_EXAMINATION_CAP {
                return Ok(Walked::Declined);
            }
            let address = RecordAddress::new(index.namespace, index.database, index.table, id);
            let Some(payload) = transaction.get(&address)? else {
                continue;
            };
            let record = self.record_of(&payload, &visible)?;
            if let Some(condition) = condition {
                let passed =
                    self.evaluate_in(transaction, condition, scope.with(&address.id, &record))?;
                if !boolean(&passed, condition.span)? {
                    continue;
                }
            }
            let measured =
                self.evaluate_in(transaction, wanted.key, scope.with(&address.id, &record))?;
            // `NONE` is a distance that did not converge, which sorts below every
            // number: a walk ranking it would put it where the scan does not.
            let Value::Number(metres) = measured else {
                return Ok(Walked::Declined);
            };
            let Some(metres) = metres.as_float().filter(|metres| metres.is_finite()) else {
                return Ok(Walked::Declined);
            };
            keep(&mut held, (metres, address.id, record), wanted.wanted);
        }
        if held.len() < wanted.wanted {
            return Ok(Walked::Declined);
        }
        Ok(Walked::Served {
            found: held
                .into_iter()
                .map(|(_, id, record)| (id, record))
                .collect(),
            index: index.name,
        })
    }
}

/// Keep `found` among the nearest `wanted`, sorted by distance and identity, the
/// whole group at the last distance kept with it.
fn keep(held: &mut Vec<(f64, RecordId, Value)>, found: (f64, RecordId, Value), wanted: usize) {
    let at = held.partition_point(|(metres, id, _)| {
        metres
            .total_cmp(&found.0)
            .then_with(|| id.cmp(&found.1))
            .is_lt()
    });
    held.insert(at, found);
    if let Some((edge, _, _)) = held.get(wanted.saturating_sub(1)) {
        let edge = *edge;
        if held.len() > wanted {
            held.retain(|(metres, _, _)| *metres <= edge);
        }
    }
}
