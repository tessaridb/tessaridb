//! The request header every request starts with, and the response header.
//!
//! The header's own version follows from the request's: a flexible request
//! carries header version 2 (tagged fields after the client id), anything else
//! version 1. The client id keeps the classic `int16` length even in version 2.
//! The response header carries tagged fields when the response is flexible —
//! except ApiVersions, whose response header is always version 0, so that a
//! client that asked with a version this node does not speak can still read
//! the answer that tells it which ones it does.

use crate::api::{self, API_VERSIONS};
use crate::codec::{Reader, Writer};
use crate::error::Result;

/// A request's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHeader<'a> {
    /// The API.
    pub key: i16,
    /// Its version.
    pub version: i16,
    /// The id the response must carry back.
    pub correlation: i32,
    /// The client's name for itself.
    pub client_id: Option<&'a str>,
    /// Whether the body (and so this header) is flexible.
    pub flexible: bool,
}

impl<'a> RequestHeader<'a> {
    /// Read the header at the start of a request frame; the reader is left at
    /// the body.
    ///
    /// An API or version this node does not answer is still read — the
    /// caller answers it — and is read as not flexible, which is what the
    /// header of every version below 2 is.
    ///
    /// # Errors
    ///
    /// What the reader refuses.
    pub fn read(reader: &mut Reader<'a>) -> Result<Self> {
        let key = reader.i16("request_api_key")?;
        let version = reader.i16("request_api_version")?;
        let correlation = reader.i32("correlation_id")?;
        let client_id = reader.nullable_string(false, "client_id")?;
        let flexible =
            api::served(key).is_some_and(|api| api.serves(version) && api.is_flexible(version));
        if flexible {
            reader.tagged_fields("request header")?;
        }
        Ok(Self {
            key,
            version,
            correlation,
            client_id,
            flexible,
        })
    }
}

/// Write a response header for `request`.
pub fn write_response_header(writer: &mut Writer, request: &RequestHeader<'_>) {
    writer.i32(request.correlation);
    if request.flexible && request.key != API_VERSIONS {
        writer.no_tagged_fields();
    }
}
