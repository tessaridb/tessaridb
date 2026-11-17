//! Encoding what the surface answers: the authentication cycle, the session
//! facts, and the rows. Every writer returns the frame's bytes complete with
//! the kind byte and the length word.

/// The transaction status a `ReadyForQuery` carries, the same three states
/// the engine's session knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tx {
    /// No transaction is open.
    Idle,
    /// A transaction is open and healthy.
    InTx,
    /// A transaction is open and the last statement failed inside it.
    Failed,
}

impl Tx {
    /// The protocol's byte for the state.
    const fn byte(self) -> u8 {
        match self {
            Self::Idle => b'I',
            Self::InTx => b'T',
            Self::Failed => b'E',
        }
    }
}

/// One column of a `RowDescription`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// The name the client sees; unnamed output columns read `?column?`, the
    /// dialect's own spelling for them.
    pub name: String,
    /// The type's object id, as the type correspondence decides.
    pub type_oid: u32,
    /// The type's size in bytes, negative for the variable-width ones.
    pub type_len: i16,
    /// The format the column crosses in: zero is text, one is the binary
    /// subset the type correspondence speaks.
    pub format: u16,
}

/// The authentication exchange is over.
pub(crate) fn authentication_ok() -> Vec<u8> {
    frame(b'R', &0_u32.to_be_bytes())
}

/// The password travels next, in cleartext inside the negotiated TLS.
pub(crate) fn cleartext_password() -> Vec<u8> {
    frame(b'R', &3_u32.to_be_bytes())
}

/// One session fact, reported at startup and on change.
pub(crate) fn parameter_status(name: &str, value: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(name.len().saturating_add(value.len()).saturating_add(2));
    cstr(&mut body, name);
    cstr(&mut body, value);
    frame(b'S', &body)
}

/// The cancel key a `CancelRequest` must repeat; cancellation itself is not
/// spoken yet, so the key names a connection nothing can cancel — the request
/// is answered by closing, per the protocol.
pub(crate) fn backend_key_data(process: u32, key: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&process.to_be_bytes());
    body.extend_from_slice(&key.to_be_bytes());
    frame(b'K', &body)
}

/// The session may take a query again.
pub(crate) fn ready_for_query(tx: Tx) -> Vec<u8> {
    frame(b'Z', &[tx.byte()])
}

/// An error: the five fields the client's rendering expects — severity,
/// severity again unlocalized, the five-character code, the message, and the
/// detail — in any order the reader accepts, so the order is the one a reader
/// prints most fluently in.
pub(crate) fn error(code: &str, message: &str, detail: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    field(&mut body, b'S', "ERROR");
    field(&mut body, b'V', "ERROR");
    field(&mut body, b'C', code);
    field(&mut body, b'M', message);
    if let Some(detail) = detail {
        field(&mut body, b'D', detail);
    }
    body.push(0);
    frame(b'E', &body)
}

/// A warning that does not stop the conversation.
pub(crate) fn notice(code: &str, message: &str) -> Vec<u8> {
    let mut body = Vec::new();
    field(&mut body, b'S', "WARNING");
    field(&mut body, b'V', "WARNING");
    field(&mut body, b'C', code);
    field(&mut body, b'M', message);
    body.push(0);
    frame(b'N', &body)
}

/// The shape of the rows that follow.
pub(crate) fn row_description(columns: &[Column]) -> Vec<u8> {
    let mut body = Vec::new();
    put_u16(&mut body, columns.len());
    for column in columns {
        cstr(&mut body, &column.name);
        // The table and column a field belongs to: zero, because the engine's
        // answers do not carry the catalog positions this field resolves by.
        body.extend_from_slice(&0_u32.to_be_bytes());
        body.extend_from_slice(&0_u16.to_be_bytes());
        body.extend_from_slice(&column.type_oid.to_be_bytes());
        body.extend_from_slice(&column.type_len.to_be_bytes());
        body.extend_from_slice(&(-1_i32).to_be_bytes());
        body.extend_from_slice(&column.format.to_be_bytes());
    }
    frame(b'T', &body)
}

/// One row; `None` is the dialect's NULL, the absence the wire spells as the
/// length -1. Cells arrive already encoded in their column's format — text
/// bytes or the binary subset's bytes.
pub(crate) fn data_row(cells: &[Option<Vec<u8>>]) -> Vec<u8> {
    let mut body = Vec::new();
    put_u16(&mut body, cells.len());
    for cell in cells {
        match cell {
            None => body.extend_from_slice(&(-1_i32).to_be_bytes()),
            Some(bytes) => {
                let len = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
                body.extend_from_slice(&len.to_be_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }
    frame(b'D', &body)
}

/// The command is complete; the tag names what ran, with the row count a
/// `SELECT` reports.
pub(crate) fn command_complete(tag: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(tag.len().saturating_add(1));
    cstr(&mut body, tag);
    frame(b'C', &body)
}

/// An Execute with a row ceiling answered what fits and holds the rest —
/// the readiness waits for the Sync, as the protocol counts.
pub(crate) fn portal_suspended() -> Vec<u8> {
    frame(b's', &Vec::new())
}

/// The query string held nothing but whitespace.
pub(crate) fn empty_query() -> Vec<u8> {
    frame(b'I', &[])
}

/// A Parse landed; the statement is prepared under its name.
pub(crate) fn parse_complete() -> Vec<u8> {
    frame(b'1', &[])
}

/// A Bind landed; the portal holds its values.
pub(crate) fn bind_complete() -> Vec<u8> {
    frame(b'2', &[])
}

/// A Close landed; whatever it named is gone.
pub(crate) fn close_complete() -> Vec<u8> {
    frame(b'3', &[])
}

/// The parameter types a statement answers Describe(statement) with: what the
/// Parse declared, the unspecified ones read as text.
pub(crate) fn parameter_description(types: &[u32]) -> Vec<u8> {
    let mut body = Vec::new();
    put_u16(&mut body, types.len());
    for held in types {
        body.extend_from_slice(&held.to_be_bytes());
    }
    frame(b't', &body)
}

/// The thing a Describe asked about carries no row shape: a statement whose
/// projection the surface cannot name without running it answers this.
pub(crate) fn no_data() -> Vec<u8> {
    frame(b'n', &[])
}

/// One frame: the kind byte, the length word counting itself and the body,
/// then the body.
fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len().saturating_add(4)).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(body.len().saturating_add(5));
    out.push(kind);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn cstr(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

fn field(out: &mut Vec<u8>, kind: u8, value: &str) {
    out.push(kind);
    cstr(out, value);
}

fn put_u16(out: &mut Vec<u8>, n: usize) {
    let n = u16::try_from(n).unwrap_or(u16::MAX);
    out.extend_from_slice(&n.to_be_bytes());
}
