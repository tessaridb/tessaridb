use super::*;

impl<'a> Session<'a> {
    /// Run a script that another node sent on its caller's behalf (ADR-0108 D2).
    ///
    /// As [`Session::run_with`], in the namespace and database the caller's
    /// own session had selected, except that a script holding any statement
    /// that changes authority or membership, or reads or replaces the whole
    /// store, is refused whole before anything in it runs.
    ///
    /// The selection is put in front of the script as a `USE` statement that is
    /// BUILT rather than written, so names another node sent can never become
    /// syntax, and so the tenancy rule a `USE` carries is applied to it.
    ///
    /// # Errors
    ///
    /// [`Error::MayNotTravel`] naming the first such statement; otherwise as
    /// [`Session::run_with`].
    pub fn run_coordinated(
        &mut self,
        (namespace, database): (Option<&str>, Option<&str>),
        source: &str,
        parameters: &Parameters,
    ) -> Result<Vec<Outcome>> {
        let mut script = parse(source)?;
        if let Some(statement) = script
            .statements
            .iter()
            .find_map(|statement| crate::administration::stays_home(&statement.kind))
        {
            return Err(Error::MayNotTravel { statement });
        }
        if namespace.is_some() || database.is_some() {
            let span = tessari_ql::Span { start: 0, end: 0 };
            let named = |text: Option<&str>| {
                text.map(|text| tessari_ql::Name {
                    text: text.to_owned(),
                    span,
                })
            };
            script.statements.insert(
                0,
                tessari_ql::Statement {
                    kind: StatementKind::Use {
                        namespace: named(namespace),
                        database: named(database),
                        consumer: None,
                    },
                    span,
                    acknowledge: None,
                    across: false,
                },
            );
        }
        let ran = self.run_script(script.bind(parameters)?)?;
        // The built `USE` answered too; the caller sent none and is owed none.
        Ok(if namespace.is_some() || database.is_some() {
            ran.into_iter().skip(1).collect()
        } else {
            ran
        })
    }

    /// Run a script that is already parsed and bound, exactly as
    /// [`Session::run_with`] runs one it read.
    ///
    /// Shared with the vault surface, whose statements are built rather than
    /// read so that a passphrase is never text (ADR-0092 D2).
    pub(crate) fn run_script(&mut self, mut script: tessari_ql::Script) -> Result<Vec<Outcome>> {
        let store = self.store;

        // Where a statement may run, asked once for the whole script and before
        // any of it runs — a script that writes must not have its first half
        // committed here and its second half refused.
        //
        // **A read pays nothing for the cluster.** The roles live in the store,
        // so consulting them costs a read, and a node standing alone would pay
        // it on every `SELECT` for an answer that is always yes. `Effect` is
        // pure, so asking it first keeps that cost on the writes it belongs to
        // (ADR-0018's `Alone`, and G008 kill criterion 3).
        if matches!(Effect::of_script(&script), Effect::Write) {
            admits(store.node_identity()?.roles, &script)?;
        }

        let mut open: Option<(Transaction<'a>, tessari_ql::Span)> = None;
        let outcomes = self.run_statements(store, &mut open, &mut script)?;

        if let Some((transaction, span)) = open {
            transaction.rollback();
            return Err(Error::UnclosedTransaction { span });
        }
        Ok(outcomes)
    }

    /// Run a parsed, bound script's statements against `open`, which is the
    /// transaction they join when one is open.
    ///
    /// Shared by [`Session::run_with`], where `open` starts empty and the script
    /// opens and closes its own, and [`Session::atomically`], where the caller
    /// holds it across several scripts.
    pub(super) fn run_statements(
        &mut self,
        store: &'a Store,
        open: &mut Option<(Transaction<'a>, tessari_ql::Span)>,
        script: &mut tessari_ql::Script,
    ) -> Result<Vec<Outcome>> {
        let mut outcomes = Vec::with_capacity(script.statements.len());
        self.landed = false;

        // By index rather than by iterator, because a `LET` reaches forward: the
        // value it produces is substituted into the statements that have not run
        // yet, so the loop holds `&mut script` across the step.
        let mut at = 0usize;
        while at < script.statements.len() {
            let outcome = self.step(store, open, &script.statements[at])?;
            // A write outside a transaction committed as it ran, and a `COMMIT`
            // committed what the transaction held. A write inside an open
            // transaction has not landed yet: refused at its `COMMIT`, it rolls
            // back with everything beside it.
            let kind = &script.statements[at].kind;
            if matches!(kind, StatementKind::Commit)
                || (open.is_none() && Effect::of(kind) == Effect::Write)
            {
                self.landed = true;
            }
            let outcome = match &script.statements[at].kind {
                StatementKind::Let { name, .. } => {
                    // Substitution, not a lookup table — the same walk the
                    // caller's parameters take, for the same reason: by the time
                    // a statement runs, every name in it is a literal, so the
                    // planner still finds a right-hand side an index can serve.
                    let bound = match outcome {
                        Outcome::Value(value) => value,
                        // Unreachable while `execute` answers a binding with a
                        // value, and named rather than unwrapped so that a
                        // change there is a compile-time conversation.
                        _ => {
                            return Err(Error::BindingIsNotAValue {
                                span: script.statements[at].span,
                            });
                        }
                    };
                    let name = name.clone();
                    let mut supplied = Parameters::new();
                    supplied.insert(name, bound);
                    for later in &mut script.statements[at.saturating_add(1)..] {
                        later.substitute(&supplied)?;
                    }
                    Outcome::Done
                }
                _ => outcome,
            };
            outcomes.push(outcome);
            at = at.saturating_add(1);
        }
        Ok(outcomes)
    }

    /// Whether the script run last committed anything before it ended.
    ///
    /// Read by the wire and HTTP surfaces before a refusal becomes a redirect
    /// (ADR-0101 D3): a client told to go elsewhere sends the whole script
    /// again, so a script that has already committed part of itself is answered
    /// with the refusal instead.
    #[must_use]
    pub const fn landed(&self) -> bool {
        self.landed
    }
}
