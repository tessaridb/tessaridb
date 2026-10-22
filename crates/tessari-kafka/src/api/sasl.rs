//! SaslHandshake and SaslAuthenticate: PLAIN, as a store user (ADR-0129 D3).

use crate::codec::{Reader, Writer};
use crate::error::{Error, Result};

/// The one mechanism this node offers.
pub const PLAIN: &str = "PLAIN";

/// The protocol's code for a mechanism the broker does not offer.
pub const UNSUPPORTED_SASL_MECHANISM: i16 = 33;
/// The protocol's code for credentials that do not sign in.
pub const SASL_AUTHENTICATION_FAILED: i16 = 58;
/// The protocol's code for a request out of order in the SASL exchange.
pub const ILLEGAL_SASL_STATE: i16 = 34;

/// Read a handshake's mechanism.
///
/// # Errors
///
/// What the reader refuses.
pub fn read_handshake<'a>(reader: &mut Reader<'a>) -> Result<&'a str> {
    reader.string(false, "mechanism")
}

/// Write a handshake answer.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_handshake(writer: &mut Writer, error_code: i16) -> Result<()> {
    writer.i16(error_code);
    writer.array_len(false, Some(1))?;
    writer.string(false, PLAIN)
}

/// Read an authenticate request's token.
///
/// # Errors
///
/// What the reader refuses.
pub fn read_authenticate<'a>(reader: &mut Reader<'a>, flexible: bool) -> Result<&'a [u8]> {
    let bytes = reader.bytes(flexible, "auth_bytes")?;
    if flexible {
        reader.tagged_fields("sasl authenticate")?;
    }
    Ok(bytes)
}

/// Write an authenticate answer at `version`.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_authenticate(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    error_code: i16,
    message: Option<&str>,
) -> Result<()> {
    writer.i16(error_code);
    writer.nullable_string(flexible, message)?;
    writer.bytes(flexible, &[])?;
    if version >= 1 {
        writer.i64(0);
    }
    if flexible {
        writer.no_tagged_fields();
    }
    Ok(())
}

/// A PLAIN token's user and password (RFC 4616): `[authzid] NUL user NUL password`.
#[derive(Debug, PartialEq, Eq)]
pub struct Plain<'a> {
    /// The user to sign in as.
    pub user: &'a str,
    /// The password.
    pub password: &'a str,
}

impl<'a> Plain<'a> {
    /// Read a PLAIN token.
    ///
    /// An authorization identity other than empty or the user is refused:
    /// acting as somebody else is a store grant, not a login option.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] for a token that is not three NUL-separated UTF-8 parts.
    pub fn read(token: &'a [u8]) -> Result<Self> {
        let malformed = |why| Error::Malformed {
            field: "sasl plain",
            why,
        };
        let text = std::str::from_utf8(token).map_err(|_| malformed("not UTF-8"))?;
        let mut parts = text.split('\0');
        let (Some(authzid), Some(user), Some(password), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(malformed("not three NUL-separated parts"));
        };
        if user.is_empty() {
            return Err(malformed("an empty user"));
        }
        if !authzid.is_empty() && authzid != user {
            return Err(malformed("an authorization identity other than the user"));
        }
        Ok(Self { user, password })
    }
}
