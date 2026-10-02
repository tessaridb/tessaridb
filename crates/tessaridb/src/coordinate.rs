//! Answering a request this node cannot answer, by asking the node that can
//! (ADR-0108 D1).
//!
//! The surfaces decide *when* — a request whose caller cannot follow a
//! redirect — and a process that knows its peers decides *how*, installing one
//! [`Coordinate`] on the store. Nothing here dials anything: a store with no
//! coordinator installed answers as it always did.

use tessari_storage::UserDefinition;

use crate::Parameters;

/// The surface a request arrived on, which is the shape its answer must take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// The wire protocol, at the minor its client greeted with.
    Wire {
        /// The client's minor.
        minor: u8,
    },
    /// HTTP's `POST /script`.
    Http,
}

/// One request to carry to another node.
#[derive(Debug, Clone, Copy)]
pub struct Coordination<'a> {
    /// The node that can answer it.
    pub to: [u8; tessari_storage::NODE_ID_LEN],
    /// Who the caller proved to be on this node; `None` when nobody signed in.
    pub user: Option<&'a UserDefinition>,
    /// What the caller's session had selected.
    pub namespace: Option<&'a str>,
    /// What the caller's session had selected.
    pub database: Option<&'a str>,
    /// The script, exactly as the caller sent it.
    pub script: &'a str,
    /// Its bound values.
    pub parameters: &'a Parameters,
    /// Where the caller is waiting.
    pub surface: Surface,
}

/// The answering node's answer, in the caller's surface's shape: a frame kind
/// and body for the wire, a status and body for HTTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coordinated {
    /// The wire frame kind, or the HTTP status.
    pub kind: u16,
    /// The body, verbatim.
    pub body: Vec<u8>,
}

/// Carries a request to the node that can answer it.
pub trait Coordinate: Send + Sync + std::fmt::Debug {
    /// Ask `asked.to` to answer `asked` as `asked.user`, and bring back what it
    /// said.
    ///
    /// # Errors
    ///
    /// Why the request could not be carried, in words a caller can act on; the
    /// surface answers with it in place of an answer.
    fn coordinate(&self, asked: &Coordination<'_>) -> Result<Coordinated, String>;
}
