//! Where the statements go — this process, or a node across a socket.
//!
//! # Both ends produce the same type, so parity is the shape rather than a claim
//!
//! The remote path gets [`Answer`] from the wire protocol. The embedded path
//! *converts into* it rather than being rendered by a second code path, so the
//! renderer cannot tell the two apart and there is no second place for "how a
//! decimal prints" to be decided. What the test at the end of this crate then
//! proves is the narrower and checkable thing: that the conversion is faithful.
//!
//! [`Answer`] is the right shape for both because it is already what a *client*
//! sees — a record id as the text the store spelled, an access path as the name
//! the store uses. An embedded session is a client that happens to share an
//! address space.
//!
//! # What the two do not share
//!
//! A store operation is not a statement. `--backup`, `--restore` and `--health`
//! reach past the session into the store itself, and the wire protocol carries
//! scripts — so they belong to the embedded path alone, and asking for one over
//! an address is refused in `main` rather than quietly ignored.

use bgv_db::{Db, Outcome, Session};
use bgv_db_wire::{Answer, Client, Names};

/// Somewhere statements can be run.
///
/// Fallible with a `String` rather than a shared error type: the two failures
/// have nothing in common but being worth printing — one is the store's refusal
/// and the other is a socket — and a common enum here would exist only to be
/// flattened again one line later.
pub trait Store {
    /// Run a script, and say what each statement answered.
    ///
    /// # Errors
    ///
    /// Returns the message a person should read: the store's refusal, or the
    /// reason the node could not be reached.
    fn run(&mut self, script: &str) -> Result<Vec<Answer>, String>;
}

/// A store open in this process.
pub struct Embedded<'a> {
    db: &'a Db,
    session: Session<'a>,
}

impl<'a> Embedded<'a> {
    /// One session for the whole run, the way a connection has one.
    ///
    /// # Errors
    ///
    /// Returns the store's own words when the credentials are refused. Refused
    /// here rather than at the first statement, because a session that carried
    /// on anonymously after a failed sign-in would answer a different question
    /// than the one that was asked.
    pub fn new(db: &'a Db, credentials: Option<&(String, String)>) -> Result<Self, String> {
        let mut session = db.session();
        if let Some((name, password)) = credentials {
            session
                .sign_in(name, password)
                .map_err(|held| held.to_string())?;
        }
        Ok(Self { db, session })
    }
}

impl Store for Embedded<'_> {
    fn run(&mut self, script: &str) -> Result<Vec<Answer>, String> {
        let outcomes = self.session.run(script).map_err(|held| held.to_string())?;
        Ok(outcomes
            .iter()
            .map(|outcome| {
                // The catalog walk happens only when the answer holds a
                // reference, which `names_in` decides by walking the values
                // first. The node does exactly this before encoding.
                into_answer(outcome, bgv_db_wire::names_for(self.db, outcome))
            })
            .collect())
    }
}

/// A node reached over the wire protocol.
pub struct Remote {
    client: Client,
    credentials: Option<(String, String)>,
}

impl Remote {
    /// Connect, and remember who to say we are.
    ///
    /// # Errors
    ///
    /// Returns the reason the node could not be reached or did not speak this
    /// protocol.
    pub fn connect(address: &str, credentials: Option<(String, String)>) -> Result<Self, String> {
        Ok(Self {
            client: Client::connect(address).map_err(|held| format!("{address}: {held}"))?,
            credentials,
        })
    }
}

impl Store for Remote {
    fn run(&mut self, script: &str) -> Result<Vec<Answer>, String> {
        // Sent with every request rather than once, so the client holds no
        // notion of being signed in that the node could disagree with. The node
        // keeps one session per connection, so what a `USE` selected is still
        // selected here.
        let credentials = self
            .credentials
            .as_ref()
            .map(|(name, password)| (name.as_str(), password.as_str()));
        self.client
            .run(script, credentials)
            .map_err(|held| held.to_string())
    }
}

/// An outcome, as a client would have received it.
fn into_answer(outcome: &Outcome, names: Names) -> Answer {
    match outcome {
        Outcome::Records { records, path } => Answer::Records {
            records: records
                .iter()
                .map(|(id, held)| (id.to_string(), held.clone()))
                .collect(),
            path: path.name().to_owned(),
            names,
        },
        Outcome::Value(held) => Answer::Value {
            value: held.clone(),
            names,
        },
        Outcome::Keys(keys) => Answer::Keys(keys.iter().map(ToString::to_string).collect()),
        Outcome::Removed { count } => Answer::Removed(*count),
        Outcome::Done => Answer::Done,
        // `Outcome` is `#[non_exhaustive]`, so a shape added to the store and
        // not to this match arrives here. Saying so is honest; guessing at its
        // content would not be — and the wire protocol answers the same way.
        _ => Answer::Unknown,
    }
}
