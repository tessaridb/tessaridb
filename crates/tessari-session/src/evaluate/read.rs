//! A read: the statement, resuming it past a cursor, and shaping what it answers.

use tessari_ql::{Projection, Select};
use tessari_storage::Transaction;
use tessari_types::{RecordId, Value};

use crate::budget::{Budget, Ceiling, Deadline};
use crate::consume::Consumer;
use crate::error::{Error, Result};
use crate::noticed::Noticed;
use crate::outcome::Note;
use crate::session::Session;

use super::{
    Answered, Prepared, Reporting, Shaped, alone, asserted, groups, held_bound, opened,
    order_bound, sought, streams,
};

impl Session<'_> {
    /// A read, as the records it found, shaped by what it asked for.
    ///
    /// The projection is applied here — once, above the four sources — so a
    /// record read by identity, by scan, by index and by traversal all answer in
    /// the same shape. Four applications would be four chances for one of them
    /// to differ.
    ///
    /// It does not change the access path. A projection that an index could
    /// answer without touching the record is a covering read, which is a
    /// planner's decision about how to run the statement rather than a change to
    /// what the statement says.
    pub(crate) fn read(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
        within: Option<Deadline>,
        holding: Option<Ceiling>,
    ) -> Result<Answered> {
        // The searched context is resolved before the source produces anything,
        // because a sort key is an expression too and one holding a `MATCHES` or
        // a score must mean the same thing there as it does in the `WHERE` that
        // produced them. That is the whole reason the source is split in two: it
        // knew `searched` before it walked, and only reported it on return, so
        // the consumer could not be built until every record already existed.
        // The one note channel for the whole read, threaded rather than kept on
        // the session: a buffer on `self` would outlive the statement that filled
        // it, and a note reported against the *next* answer is worse than no note
        // at all.
        let mut notes = Vec::new();
        // Authority first, and staleness second, because the order is the one
        // that cannot contradict itself. A node holding `Roles::WRITABLE`
        // answers `current_as_of` as ZERO, so the leader satisfies every
        // freshness bound trivially — deciding authority first therefore never
        // overturns the freshness decision, while the other order can: a
        // follower inside the bound would be chosen, and the read that said it
        // had to come from the leader would be answered by one that does not.
        if let Some(asked) = select.answered_by
            && asked.admits == tessari_ql::Admitted::Leader
            && !self
                .store
                .effective_roles()?
                .has(tessari_encoding::Roles::WRITABLE)
        {
            return Err(
                match self.elsewhere.as_ref().and_then(|known| known.writable()) {
                    // C-07 again, and unchanged by the second axis: this node
                    // names the one that should answer and does not fetch on
                    // the client's behalf.
                    Some(peer) => Error::ReadIsElsewhere {
                        because: "`ANSWERED BY LEADER`".to_owned(),
                        endpoint: peer.endpoint,
                        node: peer.node,
                        epoch: peer.epoch,
                        span: asked.span,
                    },
                    None => Error::NoLeaderKnown { span: asked.span },
                },
            );
        }
        // Before anything is read, and here rather than in the parser: the floor
        // is a fact about the cluster and the parser has no cluster. A subquery
        // carrying its own bound is checked by the same line, because it asks
        // the same impossible thing.
        if let Some(bound) = select.staleness {
            let floor = tessari_constants::STALENESS_FLOOR_SECONDS;
            // The parser has already refused a bound of zero or less, so the
            // only comparison left here is against the floor. A sub-second
            // remainder can only widen the bound, never narrow it, so whole
            // seconds decide it.
            if bound.within.seconds() < i64::try_from(floor).unwrap_or(i64::MAX) {
                return Err(Error::StalenessBelowFloor {
                    written: bound.within.to_literal(),
                    floor,
                    span: bound.span,
                });
            }
            // The bound clears the floor, so it is one this cluster could in
            // principle honour. Whether it can is a question about copies rather
            // than about grammar, and §C-05 answers it by EXCLUDING: a node
            // beyond the bound does not answer, and when that leaves nothing the
            // read is refused rather than sent to the leader. A node whose copy
            // has no known age is beyond every bound — see
            // `Store::current_as_of` for why that is the honest reading and not
            // a conservative one.
            //
            // Whole seconds again, and for the opposite reason to the floor's: a
            // sub-second remainder can only widen the bound, so dropping it can
            // only refuse a read that a wider bound would have admitted, which
            // is the direction this refusal is already erring.
            let within_bound =
                core::time::Duration::from_secs(u64::try_from(bound.within.seconds()).unwrap_or(0));
            if self
                .store
                .current_as_of()?
                .is_none_or(|age| age > within_bound)
            {
                // Asked only here, and asked only this. *Here* has already been
                // decided by the line above, so the directory is handed the
                // bound and nothing else — see `elsewhere.rs` for why letting it
                // re-decide a question already answered is the thing being
                // avoided. A node nobody told about peers holds `None` and
                // refuses exactly as it always has.
                let written = bound.within.to_literal();
                return Err(
                    match self
                        .elsewhere
                        .as_ref()
                        .and_then(|known| known.within(within_bound))
                    {
                        // C-07: this node names the one that should answer and
                        // does not fetch on the client's behalf.
                        Some(peer) => Error::ReadIsElsewhere {
                            because: format!("a staleness bound of {written}"),
                            endpoint: peer.endpoint,
                            node: peer.node,
                            epoch: peer.epoch,
                            span: bound.span,
                        },
                        // C-05's other half, unchanged: a read no node can
                        // satisfy is refused rather than promoted to the leader.
                        None => Error::NoCopyWithinStaleness {
                            written,
                            span: bound.span,
                        },
                    },
                );
            }
        }
        // Narrowed by this statement's own clause, and never widened by it: a
        // subquery may set a tighter ceiling than the read holding it and may
        // not set a looser one.
        let within = Deadline::under(within, select.timeout);
        // One budget for the whole read, so a two-stage path counts each record
        // once and the refusal says how far the *read* got. `holding` comes from
        // the caller and never from this statement: how long a read may take is
        // the statement's own business, but whether it is being built into
        // memory for somebody else is a fact only its caller knows.
        let mut budget = Budget::of(within, holding);
        // Owned by the read and borrowed by everything it evaluates, so the
        // lifetime rather than the discipline is what stops a note outliving the
        // statement that earned it.
        let noticed = Noticed::default();
        if let Some(latest) = &select.latest {
            self.check_latest(transaction, select, latest)?;
        }
        // A search ranks its records itself and projects them with what it
        // ranked them by; only a grouped read takes them through the stages
        // below, as an already-held source (ADR-0105 D6).
        if matches!(select.from, tessari_ql::Source::Search { .. }) && !groups(select) {
            return self.search_answer(transaction, select, &noticed, notes);
        }
        // A grouping read of a table this node holds only part of is folded on
        // the leaders when it can be (ADR-0097 D2): its groups stand in for the
        // records, and every stage after the fold runs below as it always has.
        // A read merging a rollup's sketches reads them here, beside its rows,
        // and is never folded on the leaders (ADR-0122 C5).
        let merging = self.rollup_merging(transaction, select)?;
        let leaders = if merging.is_none() {
            self.prepare_folded(transaction, select, (&mut notes, &noticed), within)?
        } else {
            None
        };
        let (prepared, searched, mut folded) = match leaders {
            Some((groups, plan)) => (
                Prepared::Held(Vec::new(), plan),
                crate::search::Searched::default(),
                Some(groups),
            ),
            None => {
                let (prepared, searched) = self.prepare_source(
                    transaction,
                    select,
                    Reporting {
                        collected: &mut notes,
                        noticed: &noticed,
                    },
                    within,
                )?;
                notes.extend(searched.rebuild_notes().iter().cloned());
                (prepared, searched, None)
            }
        };
        // The bound the sort may keep to. `bounded` is applied to the ordering
        // stage's output below, so keeping only what it will keep is an identity
        // between two adjacent stages rather than a decision about the
        // statement — which is why, unlike the bound handed to the source, this
        // one needs no whitelist of shapes (ADR-0013).
        let bound = order_bound(select);
        // Resolved before the source runs, because the ordering stage has to
        // hold it before the first record arrives — a cursor applied to the
        // finished answer would let the bound keep the records the page has
        // already handed back and then throw them away.
        let resuming = self.resuming(transaction, select)?;
        // The seek reaches the source itself, so only the walk has anything to
        // report. Pushed here rather than beside the seek because it is a
        // property of the statement's shape, which is decided once.
        if select.after.is_some() && !sought(select) {
            notes.push(Note::CursorWalked);
        }

        if streams(select) {
            // The projection folded once, above the records rather than per
            // record: a projection's constant parts are constant across every
            // record it is applied to.
            let wanted = self.shaped(transaction, select)?;
            let keys = self.folded_order(transaction, &select.order)?;
            let mut shaping = crate::consume::Shaping::new(
                self,
                wanted,
                keys,
                &searched,
                crate::shape::Topmost::of(select, bound),
                &mut budget,
                &noticed,
            );
            if let Some((id, record)) = resuming {
                shaping.resume_after(transaction, id, record)?;
            }
            let plan = self.produce_source(
                transaction,
                select,
                prepared,
                &searched,
                &mut shaping,
                Reporting {
                    collected: &mut notes,
                    noticed: &noticed,
                },
            )?;
            asserted(select, &plan)?;
            notes.extend(noticed.drained());
            let records = crate::shape::bounded(shaping.finish(), select.start, select.limit);
            alone(select, &records)?;
            return Ok(Answered {
                records,
                plan,
                notes,
                suggestion: searched.suggestion(),
            });
        }

        let mut collecting = crate::consume::Collecting::new(&mut budget, held_bound(select));
        let plan = self.produce_source(
            transaction,
            select,
            prepared,
            &searched,
            &mut collecting,
            Reporting {
                collected: &mut notes,
                noticed: &noticed,
            },
        )?;
        let mut records = collecting.finish();
        // Before anything groups, projects or sorts, so a projection and a sort
        // key both see the record rather than the reference that named it.
        if !select.fetch.is_empty() {
            // A reference carries a table and an id and not a tenancy, so it
            // resolves in the read's own database — which is also why a fetch
            // cannot reach across one (ADR-0008).
            let context = self.context(transaction, None, select.span)?;
            self.follow(
                transaction,
                &mut records,
                &select.fetch,
                context,
                &mut notes,
            )?;
        }
        // After the fetch — a reference resolved once and then opened is the
        // same answer as one opened and then resolved n times, and cheaper — and
        // before everything that counts records, because this is the stage that
        // decides how many there are.
        if let Some(route) = &select.split {
            records = opened(records, &route.path, &mut budget)?;
        }
        // On every path, the index walk's included: over records already one
        // per key it changes nothing, so the walk can only make it cheaper.
        if let Some(latest) = &select.latest {
            records = Self::newest_per_key(records, latest);
        }
        if select.fusion.is_some() {
            return self.fused_answer(
                transaction,
                select,
                records,
                (&searched, &noticed),
                (&mut budget, plan, notes),
            );
        }
        let records = if groups(select) {
            // A grouping folds many records into one, and a fold answers about
            // the group rather than about a record — so the star has nothing to
            // contribute here and the grammar has already refused one written
            // beside a fold.
            let (rows, filled, estimated) = match folded.take() {
                Some(groups) => self.grouped_from(
                    transaction,
                    groups,
                    select.projection.written(),
                    &select.group,
                    select.fill.as_ref(),
                )?,
                None => self.grouped(
                    transaction,
                    records,
                    select.projection.written(),
                    (&select.group, merging.as_ref()),
                    select.fill.as_ref(),
                )?,
            };
            if filled > 0 {
                notes.push(Note::Filled { windows: filled });
            }
            notes.extend(estimated);
            rows
        } else if let Some(wanted) = self.shaped(transaction, select)? {
            let mut projected = Vec::with_capacity(records.len());
            for (id, record) in records {
                let shaped =
                    self.project(transaction, &id, &record, &wanted, &searched, &noticed)?;
                projected.push((id, shaped));
            }
            projected
        } else {
            records
        };
        // Ordering comes after projection so that a key may name what the caller
        // can see: `SELECT address.city AS home … ORDER BY home` reads the name
        // the answer carries rather than the route it came from. A route still
        // works, because a projected record keeps the shape it was given only
        // where the projection preserved it — which is why the sort falls back
        // to the route when the name is not there.
        // A cursor read goes through the ordering stage even when it named no
        // order, because the clause supplies one: with no keys to compare,
        // `ranked` falls through to the identity, so the stage both applies the
        // cursor and answers in the store's own order. Skipping it would leave
        // the page to whatever order the source happened to produce, which is
        // the one thing a resumed read cannot be built on.
        let records = if select.order.is_empty() && select.after.is_none() {
            records
        } else {
            // The same ordering stage the streaming path uses, fed from a vector
            // instead of from the source. One implementation rather than two,
            // because two would be two chances for a `LIMIT` to change which
            // records an order answers with.
            //
            // `None` for the projection: these records have already been through
            // it, since the barrier that forced this path ran above.
            let keys = self.folded_order(transaction, &select.order)?;
            let mut shaping = crate::consume::Shaping::new(
                self,
                None,
                keys,
                &searched,
                crate::shape::Topmost::of(select, bound),
                &mut budget,
                &noticed,
            );
            if let Some((id, record)) = resuming {
                shaping.resume_after(transaction, id, record)?;
            }
            for (id, record) in records {
                if shaping.take(transaction, id, record)?.is_break() {
                    break;
                }
            }
            shaping.finish()
        };
        asserted(select, &plan)?;
        notes.extend(noticed.drained());
        let records = crate::shape::bounded(records, select.start, select.limit);
        alone(select, &records)?;
        Ok(Answered {
            records,
            plan,
            notes,
            suggestion: searched.suggestion(),
        })
    }

    /// The record a cursor resumes after: its identity, and itself when the
    /// order needs a key evaluated over it.
    ///
    /// The asymmetry in the middle is the whole of this function. A read that
    /// named **no order** resumes in the store's own, where the identity *is*
    /// the key — so nothing is read, and a page walk survives the deletion of
    /// the record it resumed from, which is the ordinary way a long walk ends
    /// otherwise. A read that named **an order** needs the anchor's value for
    /// that key, and there is nowhere to get it but the record: a missing anchor
    /// is refused rather than guessed at, because every guess picks a page.
    pub(super) fn resuming(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<Option<(RecordId, Option<Value>)>> {
        let Some(anchor) = &select.after else {
            return Ok(None);
        };
        let (_, address) = self.address(transaction, anchor)?;
        if select.order.is_empty() {
            return Ok(Some((address.id, None)));
        }
        let visible = self.visible_in(transaction, address.table)?;
        let Some(payload) = transaction.get(&address)? else {
            return Err(Error::AnchorGone {
                table: anchor.table.name.text.clone(),
                span: anchor.span,
            });
        };
        let record = self.record_of(&payload, &visible)?;
        Ok(Some((address.id, Some(record))))
    }

    /// What this read's projection produces, or nothing when it produces the
    /// record unchanged.
    ///
    /// `None` is the read that copies nothing — a bare `*` with no `OMIT` — and
    /// it is the commonest read in the language, so it keeps the path it had:
    /// the records reach the answer as they were decoded.
    ///
    /// The projection is folded here, once above the records, because its
    /// constant parts are constant across every record it is applied to.
    /// Rebuilding them per record is what the benchmark harness once found
    /// dominating a nearest-neighbour read.
    pub(crate) fn shaped(
        &self,
        transaction: &mut Transaction<'_>,
        select: &Select,
    ) -> Result<Option<Shaped>> {
        match &select.projection {
            Projection::All if select.omit.is_empty() => Ok(None),
            Projection::All => Ok(Some(Shaped {
                everything: true,
                omit: select.omit.clone(),
                values: Vec::new(),
            })),
            Projection::Values { everything, values } => Ok(Some(Shaped {
                everything: everything.is_some(),
                omit: select.omit.clone(),
                values: self.folded_projection(transaction, values)?,
            })),
        }
    }
}
