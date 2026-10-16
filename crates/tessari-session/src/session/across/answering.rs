use super::*;

impl Session<'_> {
    /// Write one record of a transaction across leaders, as this session's
    /// user, on this node — which must lead the record's range.
    ///
    /// # Errors
    ///
    /// Every refusal a write would meet here — tenancy, authority, grants, the
    /// fence, a conflict — plus `AcrossKind` for a table whose engine has a
    /// write path of its own, and the storage refusals of ADR-0112.
    pub async fn answer_across(&mut self, asked: &AcrossAsk) -> Result<AcrossAnswer> {
        let span = Span::new(0, 0);
        let store = self.store;
        match asked {
            AcrossAsk::Prepare {
                transaction,
                coordinator,
                seen,
                writes,
            } => {
                self.may_write_records(store, writes, span).await?;
                let mut buffered = store.begin().await?;
                buffer(&mut buffered, writes);
                let waiting = self
                    .acknowledgement_in(&mut buffered, None, Some(Acknowledge::Majority), span)
                    .await?;
                let committed = buffered
                    .prepare_across(*transaction, *coordinator, *seen)
                    .await
                    .map_err(advised)?;
                Self::await_acknowledged(store, committed, waiting, span)?;
                Ok(AcrossAnswer::Prepared(committed.sequence))
            }
            AcrossAsk::Decide {
                transaction,
                record,
            } => {
                let home = record
                    .participants
                    .first()
                    .map(|participant| participant.range)
                    .ok_or(Error::Store(tessari_storage::Error::AcrossMalformed {
                        part: "decide",
                        problem: "a record that names no participant",
                    }))?;
                let mut deciding = store.begin().await?;
                let waiting = self
                    .acknowledgement_in(
                        &mut deciding,
                        Some(home),
                        Some(Acknowledge::Majority),
                        span,
                    )
                    .await?;
                let committed = deciding
                    .decide_across(*transaction, record.clone())
                    .await
                    .map_err(advised)?;
                Self::await_acknowledged(store, committed, waiting, span)?;
                Ok(AcrossAnswer::Decided(committed.sequence))
            }
            AcrossAsk::Begin {
                transaction,
                record,
                seen,
                writes,
            } => {
                self.may_write_records(store, writes, span).await?;
                let mut buffered = store.begin().await?;
                buffer(&mut buffered, writes);
                let waiting = self
                    .acknowledgement_in(&mut buffered, None, Some(Acknowledge::Majority), span)
                    .await?;
                let committed = buffered
                    .begin_across(*transaction, record.clone(), *seen)
                    .await
                    .map_err(advised)?;
                Self::await_acknowledged(store, committed, waiting, span)?;
                Ok(AcrossAnswer::Prepared(committed.sequence))
            }
            AcrossAsk::Conclude {
                transaction,
                record,
                records,
            } => {
                let home = record
                    .participants
                    .first()
                    .map(|participant| participant.range)
                    .ok_or(Error::Store(tessari_storage::Error::AcrossMalformed {
                        part: "conclude",
                        problem: "a record that names no participant",
                    }))?;
                let mut concluding = store.begin().await?;
                let waiting = self
                    .acknowledgement_in(
                        &mut concluding,
                        Some(home),
                        Some(Acknowledge::Majority),
                        span,
                    )
                    .await?;
                let committed = concluding
                    .conclude_across(*transaction, record.clone(), records)
                    .await
                    .map_err(advised)?;
                Self::await_acknowledged(store, committed, waiting, span)?;
                Ok(AcrossAnswer::Decided(committed.sequence))
            }
            AcrossAsk::Settle {
                transaction,
                coordinator,
            } => self.settle_across(*transaction, *coordinator).await,
            AcrossAsk::Lookup {
                transaction,
                coordinator,
            } => self.lookup_across(*transaction, *coordinator).await,
            AcrossAsk::Holds { transaction, range } => {
                self.holds_across(*transaction, *range, span).await
            }
            AcrossAsk::Bar {
                transaction,
                range,
                prevent,
            } => self.bar_across(*transaction, *range, *prevent, span).await,
            AcrossAsk::Forget {
                transaction,
                coordinator,
            } => {
                let mut forgetting = store.begin().await?;
                let waiting = self
                    .acknowledgement_in(
                        &mut forgetting,
                        Some(*coordinator),
                        Some(Acknowledge::Majority),
                        span,
                    )
                    .await?;
                let committed = forgetting
                    .forget_across(*transaction, *coordinator)
                    .await
                    .map_err(advised)?;
                Self::await_acknowledged(store, committed, waiting, span)?;
                Ok(AcrossAnswer::Forgotten(committed.sequence))
            }
            AcrossAsk::Resolve {
                transaction,
                committed,
                records,
                participants,
            } => {
                let resolved = store
                    .begin()
                    .await?
                    .resolve_across(*transaction, *committed, records, participants)
                    .await
                    .map_err(advised)?;
                Ok(AcrossAnswer::Resolved(
                    resolved.map(|landed| landed.sequence),
                ))
            }
        }
    }
}
