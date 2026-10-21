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
    /// Every store slot was taken; the client may try again.
    Busy,
    /// The work panicked and took the session with it; the connection ends.
    Lost,
}

/// A connection's hold on the store: its session, between requests.
pub(crate) struct Held {
    session: Option<Detached>,
}

impl Held {
    /// A fresh, anonymous session.
    pub(crate) fn anonymous(shared: &Shared) -> Self {
        Self {
            session: Some(shared.db.session().detach()),
        }
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
        let Some(session) = self.session.take() else {
            return Err(Failed::Lost);
        };
        let db = Arc::clone(&shared.db);
        let script = format!(
            "USE NAMESPACE {}; USE DATABASE {};\n{script}",
            tenancy.0, tenancy.1
        );
        let bridged = shared
            .bridge
            .call_here(session, move |held: Detached| async move {
                let mut attached = held.attach(db.store());
                let answer = attached.run_with(&script, parameters).await;
                (attached.detach(), answer)
            })
            .await;
        match bridged {
            Bridged::Answered((session, answer)) => {
                self.session = Some(session);
                answer.map_err(Failed::Refused)
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
                Ok(())
            }
            Bridged::Answered((_, Err(refused))) => Err(Failed::Refused(refused)),
            Bridged::Busy(_) => Err(Failed::Busy),
            Bridged::Panicked => Err(Failed::Lost),
        }
    }
}
