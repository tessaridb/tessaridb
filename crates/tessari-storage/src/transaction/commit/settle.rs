use super::*;

impl Transaction<'_> {
    pub(super) fn settle(mut self, settle: Settle) -> Result<Committed> {
        // A decision across leaders writes no records and is still a commit.
        if self.writes.is_empty() && !self.is_across() {
            // The log position, not this transaction's snapshot. Nothing was
            // committed, so neither answer is a position anything was written
            // at — but the return names a log position, and the snapshot stopped
            // being one when the version was separated from it (Q-614).
            let log = self
                .store
                .own_log(crate::store::UNPARTITIONED_REPORT_HOME)?;
            return Ok(Committed {
                log,
                sequence: self.store.committed_tail(log)?,
            });
        }
        // First, and after the empty check rather than before it. First because
        // a node that has run out of leadership should not be doing schema
        // validation on work it is about to refuse; after the empty check
        // because a transaction that writes nothing has nothing to fence, and
        // refusing it would make a fenced node fail its readers' commits.
        //
        // Here rather than at the statement layer so that `dry_run` rehearses
        // it — this function's own header is the argument, and a fence a
        // `VERIFY` cannot see is a refusal an operator meets for the first time
        // in production.
        //
        // Only while this node holds no placed range's line (ADR-0082): once it
        // does, a spent store lease refuses the ranges the store line governs
        // and not the ones a live line of their own does, so the question waits
        // below until the ranges are known.
        let held_lines = self.store.holds_lines();
        if !held_lines && let Some(for_the_last) = self.store.lease_spent() {
            return Err(Error::LeaseSpent { for_the_last });
        }
        // And before the store-wide question, because the store-wide question
        // returns early on a live lease and would therefore never reach a leader
        // — while a leader writing into a range somebody else leads is exactly
        // what this catches (G025 S6.1). *May this node write* and *may this
        // node write HERE* stopped being one question the moment two nodes could
        // lead two namespaces.
        let identity = self.store.node_identity()?;
        // Counted across both loops: a restart for a moved map spends the same
        // budget a lost race does, so a map that keeps moving cannot hold a
        // commit for ever.
        let mut attempt = 0_u32;
        'placed: loop {
            // Resolved once and asked twice: both admission questions are about the
            // ranges this transaction writes, and deriving them separately is how
            // two questions about one thing come to disagree about what that thing
            // was.
            let placement = self.placement()?;
            #[cfg(test)]
            if let Some(hook) = AFTER_PLACEMENT.with(|held| held.borrow_mut().take()) {
                hook(self.store);
            }
            let ranges = self.ranges_written(&placement)?;
            let placed = self.store.refuse_if_led_elsewhere(&ranges, &identity.id)?;
            // ADR-0082: each written range is judged on the line that governs it. A
            // placed range is admitted only under a live lease on its own line, and
            // what is left is the store line's, judged exactly as it always was. A
            // store with no placement puts every range on the store line.
            let on_the_store = self.admitted_on_their_lines(&ranges, &placed, &identity.id)?;
            // And the other half of *the effective role is the lease* (ADR-0064):
            // a node that takes part in deciding writes under a leadership and at
            // no other time. Asked here rather than only at the statement layer for
            // the reason the paragraph above gives — `dry_run` must rehearse it, and
            // a refusal a `VERIFY` cannot see is one an operator meets for the first
            // time in production.
            // G027 S2.3 — and the declaration is what exempts it, by the SAME
            // predicate the divergence fence and the redirect consult, with no new
            // setting anywhere. The order matters and the `&&` is load-bearing:
            // `awaiting` returns on an in-memory lease read, so a node that holds a
            // leadership never reaches the catalog lookup, and the exemption is paid
            // for only by a commit that was otherwise about to be refused.
            if !on_the_store.is_empty() {
                if let Some(for_the_last) = self.store.lease_spent() {
                    return Err(Error::LeaseSpent { for_the_last });
                }
                if self.store.awaiting(&identity.id)?
                    && !self.every_range_admits_two_writers(&on_the_store)?
                {
                    return Err(Error::NoLeadershipYet);
                }
            }
            let store_line = !held_lines || !on_the_store.is_empty();
            let mut record = self.log_record(identity.id, &placement)?;
            // The log this commit belongs to, derived from the record before the
            // loop because it cannot change between attempts: it is a property of
            // what is being written, not of the state being written onto. The
            // position is allocated from this home's counter, which is the whole of
            // what "the sequence is per-range" means at the write end.
            // And the writer. Under a leadership, the line's one log of a
            // single-leader range, which the next leader continues (ADR-0107) —
            // and THIS node's own where the range admits two writers, since two
            // masters are two counters (S2.2). Under none — a store standing
            // alone, a node's declarations before it joins — its own log. The
            // leadership is asked once here and stamped on every attempt, so the
            // log and the epoch the record names cannot disagree.
            let home = crate::catalog::home_of(&record)?;
            let epoch = self.store.epoch_under(&placed, home);
            let log = if epoch > Epoch::ZERO && !self.admits_two_writers_here(home)? {
                tessari_encoding::LogId::line(home)
            } else {
                self.store.own_log(home)?
            };

            loop {
                attempt = attempt.saturating_add(1);
                if attempt > MAX_COMMIT_ATTEMPTS {
                    tracing::error!(attempts = MAX_COMMIT_ATTEMPTS, "commit gave up");
                    return Err(Error::CommitContention {
                        attempts: MAX_COMMIT_ATTEMPTS,
                    });
                }

                // Held from reading the tail to applying the batch, so no other
                // writer in this process can move what this attempt builds on
                // (`crate::gate`). Dropped at the end of the attempt, the wait
                // before a retry included.
                let turn = self.store.write_gate().hold();
                // ADR-0095 D8: the map this attempt was admitted and stamped
                // under is asked for again under the gate, because the statement
                // that moves a map teaches the registry under this same gate. A
                // map that moved in between would file these records in a shard
                // nothing writes again, so the whole admission is redone with
                // the new one — compared by value, since a re-learn of the same
                // map is a new `Arc` and no reason to start again.
                if self.placement()? != placement {
                    drop(turn);
                    tracing::debug!(
                        attempt,
                        "a shard map moved before the commit; placing again"
                    );
                    continue 'placed;
                }
                self.refuse_if_fenced_since(store_line, &placed, &ranges)?;
                let tail = self.store.committed_tail(log)?;
                // A prepare's half of the conflict check that its own node
                // could not make (ADR-0112 D3a), under the same turn.
                self.written_since_seen(log)?;
                self.check_for_conflicts()?;
                // Beside the conflict check, inside the loop, and for the same
                // reason: both ask whether the committed state this attempt builds
                // on will take the write, and a state that moved between attempts
                // must be re-read rather than assumed.
                //
                // AFTER it rather than before, so a record that is both moved and
                // contested answers `Conflict` first. That is the retryable one, and
                // a caller that retries meets the concurrency on the next attempt —
                // which is the right order to learn them in, because a stale write
                // has nothing useful to say about a conflict it never saw.
                let discarded = self.refuse_a_contested_record()?;
                // Inside the loop with the conflict check, and for the same reason:
                // both are read against the committed state this attempt builds on,
                // and a schema that moved between attempts must be re-read rather
                // than assumed.
                crate::schema::validate(self.store, &record)?;
                // A limited space's bound, against the same committed state and for
                // the same reason: two commits adding different keys do not conflict,
                // so only a count read here, per attempt, is exact (G036). The
                // attempt writes the record with its evictions when there are any.
                let mut evicted = crate::bounded::enforce(self.store, &record, identity.id)?;
                // A topic admits its messages against the same committed state and at
                // this transaction's clock: never a rewrite, never a deletion before
                // its retention passed — judged as a reader would judge it — never a
                // message over its size, and each new one carrying its expiry (G037).
                if let Some(admitted) = crate::topic::admit(
                    self.store,
                    evicted.as_ref().unwrap_or(&record),
                    self.clock(),
                    identity.id,
                )? {
                    evicted = Some(admitted);
                }
                let carried = match evicted.as_mut() {
                    Some(carrying) => carrying,
                    None => &mut record,
                };

                // Deciding the sequence locally is the *only* thing a commit does
                // that a replica's apply does not. Everything after this line is the
                // shared path.
                let commit_at = Sequence::new(tail.get().saturating_add(1));
                // And the version, separately, because it is a different fact: the
                // position is what a replica resumes from and compares, the version
                // is where this store's own history puts these records. Read inside
                // the loop for the same reason the tail is — a lost attempt built on
                // a state that has since moved (Q-614).
                let commit_version =
                    Sequence::new(self.store.committed_version()?.get().saturating_add(1));
                // And the version is the writer's ORDER, written into the record:
                // this node files its commits in one log per home, and a follower
                // applying those logs needs to know where each commit stood among
                // all of them — which only the writer knows (ADR-0084, Q-796).
                carried.set_order(commit_version);
                // And the leadership it is committed under, on the line that
                // governs its home — the epoch a follower refuses a second
                // history by and an election compares (ADR-0059); zero for a
                // node nobody made a leader.
                carried.set_epoch(epoch);
                let written = crate::log::apply_batch(log, commit_at, commit_version, carried);
                // A record of a transaction across leaders is checked and settled
                // here as a follower's apply settles it, and an intent derives
                // nothing until its resolution does (ADR-0112).
                let written = crate::intents::settle(
                    self.store,
                    carried,
                    written,
                    commit_version,
                    Some((log, commit_at)),
                )?;
                let batch = if crate::intents::derives_nothing(carried) {
                    written
                } else {
                    // Index entries are derived here rather than carried in the record,
                    // and they are derived inside the loop because they depend on the
                    // committed state this attempt is building on (see `crate::index`).
                    let batch = crate::index::maintain(self.store, carried, written)?;
                    // Adjacency is derived in the same place and for the same reason: a
                    // replica reaches its state by replaying this record, so entries the
                    // leader merely added to its own batch would never exist on a
                    // follower — a walk that finds nothing there while the leader is
                    // correct, with nothing in an error state.
                    let batch = crate::adjacency::maintain(self.store, carried, batch)?;
                    // And the record counts, in the same batch and for the third time
                    // for the same reason: the planner on a follower must read the same
                    // number as the planner on the leader, or one query takes two access
                    // paths depending on which node answered it.
                    let batch =
                        crate::cardinality::maintain(self.store, carried, batch, commit_version)?;
                    // The expiry index, last and in the same batch as the records it
                    // describes: an entry written anywhere else is an entry that can be
                    // left behind (G035).
                    let batch = crate::lapse::maintain(self.store, carried, batch)?;
                    // And a limited space's modified-order index, which the evictions
                    // above read on the next commit (G036).
                    let batch =
                        crate::bounded::maintain(self.store, carried, batch, commit_version)?;
                    // And a topic's positions, dense in commit order (G037).
                    let batch = crate::topic::maintain(self.store, carried, batch)?;
                    // And this node's format stamp, when the record finalizes it
                    // (ADR-0118) — on the leader as on every follower.
                    crate::format_stamp::maintain(self.store, carried, batch)?
                };
                // Everything above this ran. This is the whole difference between a
                // rehearsal and a write, and it is one line so that it can only ever
                // be the whole difference.
                if matches!(settle, Settle::Discard) {
                    return Ok(Committed {
                        log,
                        sequence: commit_at,
                    });
                }
                // Before the batch can be read: a reader at this version must not be
                // answered from name or table rows held from before it.
                // And, under the same turn, every map this commit moves is taught
                // to the registry before the turn is handed on, because the next
                // commit to hold it re-asks its placement against that registry
                // (ADR-0095 D8). Forgotten again below if the batch does not land.
                let mut taught = Vec::new();
                if crate::catalog::CatalogRows::changes(carried) {
                    self.store.catalog_rows().changed(commit_version);
                    taught = self
                        .store
                        .shards()
                        .teach(self.store.decoded_tables(), carried)?;
                }

                // Staged rather than applied when the backend shares a sync
                // between writes, and the turn handed on before the wait: the next
                // writer derives on this batch while it is on its way to the
                // device, and whoever finds no landing running lands every staged
                // batch in one write (`crate::gate`). A backend with no sync to
                // share is applied under the turn, as grouping would only cost it.
                let backend = self.store.backend().as_ref();
                let landed = if backend.groups_writes() {
                    let ticket = self.store.write_gate().stage(batch);
                    drop(turn);
                    self.store.write_gate().land(ticket, backend)
                } else {
                    let applied =
                        self.store
                            .write_gate()
                            .apply(batch, backend, crate::gate::Landing::Synced);
                    drop(turn);
                    applied
                };
                match landed {
                    Ok(()) => {
                        // Counted HERE and not where it was decided. The decision is
                        // re-taken on every attempt, so an attempt that loses its
                        // batch would otherwise count a loss it never caused — and
                        // the `Settle::Discard` return above this line skips it for
                        // the same reason, because a rehearsal discards nothing.
                        if discarded > 0 {
                            self.store.discarded(discarded);
                        }
                        return Ok(Committed {
                            log,
                            sequence: commit_at,
                        });
                    }
                    // The position moved between reading it and applying — or the
                    // batch was derived on a staged one that did not land — so the
                    // conflict check above was made against a stale state and the
                    // whole attempt is repeated rather than patched up — after
                    // waiting, so that this attempt does not re-race into the same
                    // instant as every other loser.
                    Err(tessari_kv::Error::Conflict { .. }) => {
                        self.store.shards().forget(&taught);
                        // At debug: one contended key under load produces this line
                        // per loser per attempt, and a retry that then succeeds is
                        // the design working rather than an event.
                        tracing::debug!(attempt, "commit lost an attempt; retrying");
                        back_off(attempt);
                        continue;
                    }
                    Err(other) => {
                        self.store.shards().forget(&taught);
                        return Err(other.into());
                    }
                }
            }
        }
    }
}
