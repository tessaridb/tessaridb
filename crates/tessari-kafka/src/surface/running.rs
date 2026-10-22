//! Running statements on a connection's session, through the node's bridge.

use std::sync::Arc;

use tessari_serve::Bridged;
use tessaridb::{Detached, Outcome, Parameters};

use crate::surface::Shared;

/// Why a statement did not answer.
#[derive(Debug)]
pub(crate) enum Failed {
    /// The store refused it, in the store's words and class.
    Refused(tessaridb::Error),
    /// The node it was carried to refused it, in that store's words and class
    /// (G079 W5d).
    Carried(tessaridb::carried::CarriedRefusal),
    /// Every store slot was taken; the client may try again.
    Busy,
    /// The work panicked and took the session with it; the connection ends.
    Lost,
}

/// A connection's hold on the store: its session, between requests.
pub(crate) struct Held {
    session: Option<Detached>,
    /// The store user signed in, empty for an anonymous session.
    user: String,
}

impl Held {
    /// A fresh, anonymous session.
    pub(crate) fn anonymous(shared: &Shared) -> Self {
        Self {
            session: Some(shared.db.session().detach()),
            user: String::new(),
        }
    }

    /// The store user signed in, empty for an anonymous session.
    pub(crate) fn user(&self) -> &str {
        &self.user
    }

    /// Replace the session with `session`, signed in elsewhere.
    pub(crate) fn replace(&mut self, session: Detached) {
        self.session = Some(session);
    }

    /// Run `script` with `parameters` bound, the database chosen by `namespace`
    /// and `database` first, so a statement never runs against a database a
    /// previous request left selected.
    pub(crate) async fn run(
        &mut self,
        shared: &Arc<Shared>,
        tenancy: (&str, &str),
        script: &str,
        parameters: &Parameters,
    ) -> Result<Vec<Outcome>, Failed> {
        self.run_at(shared, Some(tenancy.0), Some(tenancy.1), script, parameters)
            .await
    }

    /// Run `script` in the namespace and database given, where either may be
    /// absent for a statement about the store or a namespace; each given name
    /// must already have been checked as an identifier.
    pub(crate) async fn run_at(
        &mut self,
        shared: &Arc<Shared>,
        namespace: Option<&str>,
        database: Option<&str>,
        script: &str,
        parameters: &Parameters,
    ) -> Result<Vec<Outcome>, Failed> {
        let Some(session) = self.session.take() else {
            return Err(Failed::Lost);
        };
        let db = Arc::clone(&shared.db);
        let mut prefixed = String::with_capacity(script.len().saturating_add(64));
        if let Some(namespace) = namespace {
            prefixed.push_str("USE NAMESPACE ");
            prefixed.push_str(namespace);
            prefixed.push_str("; ");
        }
        if let Some(database) = database {
            prefixed.push_str("USE DATABASE ");
            prefixed.push_str(database);
            prefixed.push_str("; ");
        }
        prefixed.push_str(script);
        let script = prefixed;
        let bridged = shared
            .bridge
            .call_here(session, move |held: Detached| async move {
                let mut attached = held.attach(db.store());
                let answer = match attached.run_with(&script, parameters).await {
                    // Refused here because another node answers it — the leader
                    // of the topic it writes — and nothing of it landed here:
                    // carried there, as HTTP carries a script (ADR-0108 D1).
                    Err(refused) if !attached.landed() && tessaridb::travels(&script) => {
                        tessaridb::carried::carry(&db, &attached, refused, &script, parameters)
                            .await
                            .map_err(|not| match not {
                                tessaridb::carried::NotCarried::Here(refused) => {
                                    Failed::Refused(refused)
                                }
                                tessaridb::carried::NotCarried::There(refused) => {
                                    Failed::Carried(refused)
                                }
                            })
                    }
                    answered => answered.map_err(Failed::Refused),
                };
                (attached.detach(), answer)
            })
            .await;
        match bridged {
            Bridged::Answered((session, answer)) => {
                self.session = Some(session);
                answer
            }
            Bridged::Busy(session) => {
                self.session = Some(session);
                Err(Failed::Busy)
            }
            Bridged::Panicked => Err(Failed::Lost),
        }
    }

    /// Sign a fresh session in as `user`, keeping the old one on a refusal.
    pub(crate) async fn sign_in(
        &mut self,
        shared: &Arc<Shared>,
        user: &str,
        password: &str,
    ) -> Result<(), Failed> {
        let fresh = shared.db.session().detach();
        let db = Arc::clone(&shared.db);
        let user_name = user.to_owned();
        let (user, password) = (
            user.to_owned(),
            zeroize::Zeroizing::new(password.to_owned()),
        );
        let bridged = shared
            .bridge
            .call_here(fresh, move |held: Detached| async move {
                let mut attached = held.attach(db.store());
                let answer = attached.sign_in(&user, &password).await;
                (attached.detach(), answer)
            })
            .await;
        match bridged {
            Bridged::Answered((session, Ok(()))) => {
                self.replace(session);
                self.user = user_name;
                Ok(())
            }
            Bridged::Answered((_, Err(refused))) => Err(Failed::Refused(refused)),
            Bridged::Busy(_) => Err(Failed::Busy),
            Bridged::Panicked => Err(Failed::Lost),
        }
    }
}
