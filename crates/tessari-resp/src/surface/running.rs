//! Running statements on a connection's session, through the node's bridge.

use std::sync::Arc;

use tessari_serve::Bridged;
use tessaridb::{Detached, Outcome, Parameters};

use crate::surface::{Shared, Space};

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

/// A connection's hold on the store: its session, between commands.
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

    /// Run `script` in `space`'s database with `parameters` bound, answering
    /// the script's own outcomes — the `USE` that selects the database first
    /// is not one of them. The database is selected on every call, so a
    /// statement never runs where a previous command left the session.
    pub(crate) async fn run(
        &mut self,
        shared: &Arc<Shared>,
        space: &Space,
        script: &str,
        parameters: Parameters,
    ) -> Result<Vec<Outcome>, Failed> {
        let Some(session) = self.session.take() else {
            return Err(Failed::Lost);
        };
        let db = Arc::clone(&shared.db);
        let mut prefixed = String::with_capacity(
            script
                .len()
                .saturating_add(space.namespace.len())
                .saturating_add(space.database.len())
                .saturating_add(32),
        );
        prefixed.push_str("USE NAMESPACE ");
        prefixed.push_str(&space.namespace);
        prefixed.push_str("; USE DATABASE ");
        prefixed.push_str(&space.database);
        prefixed.push_str("; ");
        prefixed.push_str(script);
        let bridged = shared
            .bridge
            .call_here(session, move |held: Detached| async move {
                let mut attached = held.attach(db.store());
                let answer = attached.run_with(&prefixed, &parameters).await;
                (attached.detach(), answer)
            })
            .await;
        match bridged {
            Bridged::Answered((session, answer)) => {
                self.session = Some(session);
                answer
                    .map(|mut outcomes| outcomes.split_off(USE_OUTCOMES.min(outcomes.len())))
                    .map_err(Failed::Refused)
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
        password: &[u8],
    ) -> Result<(), Failed> {
        let fresh = shared.db.session().detach();
        let db = Arc::clone(&shared.db);
        let user = user.to_owned();
        let password = zeroize::Zeroizing::new(String::from_utf8_lossy(password).into_owned());
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
                self.session = Some(session);
                Ok(())
            }
            Bridged::Answered((_, Err(refused))) => Err(Failed::Refused(refused)),
            Bridged::Busy(_) => Err(Failed::Busy),
            Bridged::Panicked => Err(Failed::Lost),
        }
    }

    /// Go back to an anonymous session, as `RESET` does.
    pub(crate) fn forget(&mut self, shared: &Shared) {
        self.session = Some(shared.db.session().detach());
    }
}

/// The outcomes of the two `USE`s every script starts with.
const USE_OUTCOMES: usize = 2;
