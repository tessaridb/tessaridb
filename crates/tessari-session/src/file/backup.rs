use super::*;

impl Session<'_> {
    /// `READ media:'/logo.png'` — the file's bytes, or `NONE` if there is none.
    ///
    /// Read by point reads rather than by a scan: the metadata says how many
    /// chunks there are and it was written in the same commit as the chunks, so
    /// the count is not a guess that a scan would be checking.
    /// The store's log as a backup file, from `from` or from the beginning.
    ///
    /// # Why this answers with the bytes rather than writing a file
    ///
    /// Because a node that is **serving** holds the store, and this store is
    /// single-writer (ADR-0007) — so no second process can open it to take a
    /// backup. Asking the node is the only way, and the language is how this
    /// store is asked (ADR-0011 §6): the HTTP route is a surface over this
    /// statement rather than a second implementation of it, and the CLI and the
    /// wire protocol get it without one either.
    ///
    /// # What a concurrent write does to it
    ///
    /// Nothing, and that was already true: `write_from` fixes the log's tail
    /// **before** reading the first record and stops there, so a write that
    /// lands mid-backup is honestly outside the file rather than half inside it.
    /// The tail is written into the header, which is what makes "outside"
    /// checkable rather than a claim.
    ///
    /// # The cost, stated
    ///
    /// The whole file is materialised, because a statement answers with a value.
    /// `FROM` is what bounds it — an incremental backup carries the records since
    /// a sequence — and a streaming answer is named in `docs/tessariql.md` §8 rather
    /// than left to be discovered by whoever backs up a large store first.
    pub(crate) fn backup(
        &self,
        from: Option<u64>,
        form: tessari_ql::BackupForm,
        of: &[tessari_ql::ReachRef],
    ) -> Result<Outcome> {
        let mut held = Vec::new();
        if form == tessari_ql::BackupForm::Script {
            let taken = self.state_script(of)?;
            // Sealed, a script is bytes like the other forms; plain, it stays
            // the text it is.
            return Ok(Outcome::Value(match &self.at_rest {
                Some(_) => Value::Bytes(self.sealed(taken.text.into_bytes())?),
                None => Value::String(taken.text),
            }));
        }
        if form == tessari_ql::BackupForm::State {
            let within = self.state_scope(of)?;
            self.refuse_a_partial_snapshot(within)?;
            if let Some(out) = self.sink.take() {
                return self.snapshot_streamed(within, out);
            }
            tessari_backup::write_state_within(self.store, within, &mut held).map_err(|error| {
                Error::BackupFailed {
                    reason: error.to_string(),
                }
            })?;
            return Ok(Outcome::Value(Value::Bytes(self.sealed(held)?)));
        }
        // No `FROM` backs up the store, which is every log it holds. A `FROM`
        // names one sequence, and a sequence counts in one log — so it is the
        // incremental path, and a store holding several refuses it rather than
        // answering with a file that reads as whole and is missing the rest
        // (Q-624).
        match from.filter(|from| *from > 1) {
            None => tessari_backup::write(self.store, &mut held),
            Some(from) => {
                let home =
                    tessari_backup::only_log(self.store).map_err(|error| Error::BackupFailed {
                        reason: error.to_string(),
                    })?;
                tessari_backup::write_from(
                    self.store,
                    &mut held,
                    home,
                    tessari_types::Sequence::new(from),
                )
            }
        }
        .map_err(|error| Error::BackupFailed {
            reason: error.to_string(),
        })?;
        Ok(Outcome::Value(Value::Bytes(self.sealed(held)?)))
    }

    /// A backup's bytes as this node hands them out: sealed under its key when
    /// it has one (ADR-0108 D7), as they are when it has none.
    fn sealed(&self, plain: Vec<u8>) -> Result<Vec<u8>> {
        let Some(key) = &self.at_rest else {
            return Ok(plain);
        };
        let failed = |error: std::io::Error| Error::BackupFailed {
            reason: error.to_string(),
        };
        let mut sealing = key
            .seal_into(Vec::with_capacity(plain.len()))
            .map_err(failed)?;
        std::io::Write::write_all(&mut sealing, &plain).map_err(failed)?;
        sealing.finish().map_err(failed)
    }

    /// Write the snapshot of `within` into the caller's sink, and answer what
    /// it holds rather than its bytes.
    fn snapshot_streamed(
        &self,
        within: tessari_types::Reach,
        out: Box<dyn std::io::Write + Send>,
    ) -> Result<Outcome> {
        let failed = |reason: String| Error::BackupFailed { reason };
        let taken = match &self.at_rest {
            Some(key) => {
                let mut sealing = key
                    .seal_into(out)
                    .map_err(|error| failed(error.to_string()))?;
                let taken = tessari_backup::write_state_within(self.store, within, &mut sealing)
                    .map_err(|error| failed(error.to_string()))?;
                sealing
                    .finish()
                    .map_err(|error| failed(error.to_string()))?;
                taken
            }
            None => {
                let mut out = out;
                let taken = tessari_backup::write_state_within(self.store, within, &mut out)
                    .map_err(|error| failed(error.to_string()))?;
                out.flush().map_err(|error| failed(error.to_string()))?;
                taken
            }
        };
        let count = |held: u64| Value::from(i64::try_from(held).unwrap_or(i64::MAX));
        Ok(Outcome::Value(Value::Object(
            std::collections::BTreeMap::from([
                ("form".to_owned(), Value::from("state")),
                ("records".to_owned(), count(taken.records)),
                ("version".to_owned(), count(taken.version.get())),
            ]),
        )))
    }
}
