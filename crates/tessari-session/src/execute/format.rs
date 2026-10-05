//! The format a statement may write, against the one the store holds
//! (ADR-0118).

use std::collections::BTreeMap;

use tessari_encoding::FormatVersion;
use tessari_storage::{Catalog, Transaction};
use tessari_types::{Number, Value};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl Session<'_> {
    /// Refuse a statement that would write a value of `needs` into a store
    /// holding an older format.
    ///
    /// Asked where the statement runs, and never where a replicated record is
    /// applied: the leader decides whether the newer value is written at all,
    /// and a replica applying it has already been told it was.
    pub(super) fn refuse_a_format_the_store_does_not_hold(
        &self,
        what: &'static str,
        needs: FormatVersion,
    ) -> Result<()> {
        let holds = self.store.held_format()?;
        if holds < needs {
            return Err(Error::FormatNotFinalized {
                what,
                needs: needs.get(),
                holds: holds.get(),
            });
        }
        Ok(())
    }
}

impl Session<'_> {
    /// `ALTER STORE FINALIZE FORMAT` — raise the format the store holds to the
    /// one this build writes, on every replica (ADR-0118 D3).
    ///
    /// Answers the format the store holds afterwards. Nothing is written when
    /// it already holds this build's format: a finalize never lowers, and one
    /// that changes nothing is not a write anybody needs replicated.
    pub(super) fn finalize_format(&self, transaction: &mut Transaction<'_>) -> Result<Outcome> {
        let target = FormatVersion::CURRENT;
        let holds = self.store.held_format()?;
        if holds < target {
            self.refuse_a_peer_that_cannot_read(transaction, target)?;
            Catalog::new(transaction).finalize_format(target);
        }
        Ok(Outcome::Value(Value::Object(BTreeMap::from([(
            "format".to_owned(),
            Value::Number(Number::Integer(i64::from(holds.max(target).get()))),
        )]))))
    }

    /// Every declared replica but this node must have greeted it running a
    /// release that writes `target`.
    fn refuse_a_peer_that_cannot_read(
        &self,
        transaction: &mut Transaction<'_>,
        target: FormatVersion,
    ) -> Result<()> {
        let me = self.store.node_identity()?.id;
        let needs = target.first_written_by().ok_or(Error::FormatNotFinalized {
            what: "a finalize",
            needs: target.get(),
            holds: self.store.held_format()?.get(),
        })?;
        for replica in Catalog::new(transaction).replicas()? {
            if replica.node == Some(me) {
                continue;
            }
            let heard = replica
                .node
                .and_then(|node| self.store.follower_build(&node));
            if heard.is_none_or(|build| build < needs) {
                return Err(Error::FormatPeerTooOld {
                    name: replica.name,
                    heard: heard.map_or_else(
                        || "has not reported its build to this node".to_owned(),
                        |build| format!("runs {build}"),
                    ),
                    format: target.get(),
                    needs: needs.to_string(),
                });
            }
        }
        Ok(())
    }
}
