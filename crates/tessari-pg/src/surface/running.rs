//! Running statements on a connection's session, through the node's bridge —
//! the same hold the RESP surface keeps: one detached session per connection,
//! every script prefixed with its tenancy so a statement never runs where a
//! previous one left the session, and a refusal carried to the answering node
//! rather than dropped.

use std::sync::Arc;

use tessari_serve::Bridged;
use tessaridb::{Detached, Outcome, Parameters};

/// Why a statement did not answer.
#[derive(Debug)]
pub(crate) enum Failed {
    /// The store refused it, in the store's words and class.
    Refused(tessaridb::Error),
    /// The node it was carried to refused it, in that store's words and class.
    Carried(tessaridb::carried::CarriedRefusal),
    /// Every store slot was taken; the client may try again.
    Busy,
    /// The work panicked and took the session with it; the connection ends.
    Lost,
}

/// A failed run mapped the way the answering paths answer it.
pub(crate) fn run_refusal(failed: Failed) -> super::errors::Refused {
    match failed {
        Failed::Refused(refused) => super::errors::Refused::store(&refused),
        Failed::Carried(carried) => super::errors::Refused::carried(carried.class, carried.message),
        Failed::Busy => super::errors::Refused::surface(
            super::errors::code::UNAVAILABLE,
            "the node is busy; try again",
        ),
        Failed::Lost => {
            super::errors::Refused::surface(super::errors::code::INTERNAL, "the session is lost")
        }
    }
}

/// A connection's hold on the store: its session and the tenancy every
/// statement runs under, chosen when the wire session started.
pub(crate) struct Held {
    session: Option<Detached>,
    namespace: String,
    database: String,
    /// The user the connection signed in as, when the startup named one —
    /// what the catalog's owner answers ride on.
    user: Option<String>,
    /// Whether the session parked a transaction a `BEGIN` opened — the
    /// engine's side of the connection's block state, read after every run
    /// for the readiness the wire reports.
    transaction_open: bool,
}

impl Held {
    /// A fresh, anonymous session under `namespace`.`database`.
    pub(crate) fn anonymous(shared: &super::Shared, namespace: &str, database: &str) -> Self {
        Self {
            session: Some(shared.db.session().detach()),
            namespace: namespace.to_owned(),
            database: database.to_owned(),
            user: None,
            transaction_open: false,
        }
    }

    /// The tenancy prefix every script carries, so a statement never runs
    /// where a previous statement left the session.
    fn prefix(&self) -> String {
        format!(
            "USE NAMESPACE {}; USE DATABASE {}; ",
            self.namespace, self.database
        )
    }

    /// The namespace the connection reads under — what `search_path` answers
    /// and what `SET search_path` changes.
    pub(crate) fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The database the connection reads under — the synthesis salt for the
    /// user-table identities.
    pub(crate) fn database(&self) -> &str {
        &self.database
    }

    /// The user the connection signed in as, when the startup named one.
    pub(crate) fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// Move the connection to another namespace; the next statement's tenancy
    /// prefix carries it, so nothing runs where the last session left it.
    pub(crate) fn set_namespace(&mut self, namespace: &str) {
        self.namespace = namespace.to_owned();
    }

    /// Run `script` with `parameters` bound, answering the script's own
    /// outcomes — the two `USE`s the prefix adds are not among them.
    ///
    /// Every run rides the session's held transaction when one is parked
    /// (`run_holding`): outside a block nothing is held and this is the
    /// ordinary auto-commit path; inside one, the statements join the block
    /// the connection's `BEGIN` opened.
    pub(crate) async fn run(
        &mut self,
        shared: &Arc<super::Shared>,
        script: &str,
        parameters: &Parameters,
    ) -> Result<Vec<Outcome>, Failed> {
        let Some(session) = self.session.take() else {
            return Err(Failed::Lost);
        };
        let prefixed = format!("{}{}", self.prefix(), script);
        let db = Arc::clone(&shared.db);
        let parameters = parameters.clone();
        let bridged = shared
            .bridge
            .call_here(session, move |held: Detached| async move {
                let mut attached = held.attach(db.store());
                let answer = match attached.run_holding(&prefixed, &parameters).await {
                    Err(refused) if !attached.landed() && tessaridb::travels(&prefixed) => {
                        tessaridb::carried::carry(&db, &attached, refused, &prefixed, &parameters)
                            .await
                            .map_err(|not| match not {
                                tessaridb::carried::NotCarried::Here(refused) => {
                                    Failed::Refused(refused)
                                }
                                tessaridb::carried::NotCarried::There(carried) => {
                                    Failed::Carried(carried)
                                }
                            })
                    }
                    answered => answered.map_err(Failed::Refused),
                };
                (attached.detach(), answer)
            })
            .await;
        match bridged {
            Bridged::Answered((session, Ok(answer))) => {
                self.transaction_open = session.transaction_open();
                self.session = Some(session);
                let mut outcomes = answer.into_iter();
                outcomes.by_ref().take(USE_OUTCOMES).for_each(drop);
                Ok(outcomes.collect())
            }
            Bridged::Answered((session, Err(failed))) => {
                // A refusal ends the block: the session dropped what it held.
                self.transaction_open = session.transaction_open();
                self.session = Some(session);
                Err(failed)
            }
            Bridged::Busy(held) => {
                self.session = Some(held);
                Err(Failed::Busy)
            }
            Bridged::Panicked => Err(Failed::Lost),
        }
    }

    /// Whether the session holds an open transaction — the readiness half of
    /// the connection's block state.
    pub(crate) fn transaction_open(&self) -> bool {
        self.transaction_open
    }

    /// Sign a fresh session in as `user`, keeping the old one on a refusal.
    pub(crate) async fn sign_in(
        &mut self,
        shared: &Arc<super::Shared>,
        user: &str,
        password: &str,
    ) -> Result<(), Failed> {
        let fresh = shared.db.session().detach();
        let db = Arc::clone(&shared.db);
        let user = user.to_owned();
        let signed = user.clone();
        let password = zeroize::Zeroizing::new(password.to_owned());
        let bridged = shared
            .bridge
            .call_here(fresh, move |held: Detached| async move {
                let mut attached = held.attach(db.store());
                let answer = attached.sign_in(&signed, &password).await;
                (attached.detach(), answer)
            })
            .await;
        match bridged {
            Bridged::Answered((session, Ok(()))) => {
                self.session = Some(session);
                self.user = Some(user);
                self.transaction_open = false;
                Ok(())
            }
            Bridged::Answered((_, Err(refused))) => Err(Failed::Refused(refused)),
            Bridged::Busy(_) => Err(Failed::Busy),
            Bridged::Panicked => Err(Failed::Lost),
        }
    }
}

/// The outcomes of the two `USE`s every script starts with.
const USE_OUTCOMES: usize = 2;
