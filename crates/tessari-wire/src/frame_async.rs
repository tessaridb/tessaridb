//! The same frames as `frame.rs`, read and written without holding a thread.
//!
//! Only the node reads asynchronously; a client and the peer link keep the
//! blocking half. The header's encoding and the ceiling are not repeated here —
//! [`frame::header`] and [`frame::announced`] are the one copy of each, so the
//! two halves cannot come to disagree about what a frame is.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::frame::{self, HELLO, Kind, MAJOR, MINOR};

/// Write one frame and flush it.
///
/// # Errors
///
/// Returns an error when the stream fails or the body is above the ceiling.
pub(crate) async fn write(
    out: &mut (impl AsyncWrite + Unpin),
    kind: Kind,
    body: &[u8],
) -> Result<()> {
    write_tagged(out, kind.tag(), body).await
}

/// Write one frame under a raw tag — the peer link's tag space.
///
/// # Errors
///
/// As [`write`].
pub(crate) async fn write_tagged(
    out: &mut (impl AsyncWrite + Unpin),
    tag: u8,
    body: &[u8],
) -> Result<()> {
    out.write_all(&frame::header(tag, body)?).await?;
    out.write_all(body).await?;
    out.flush().await?;
    Ok(())
}

/// Read one frame, or `None` when the peer hung up cleanly between frames.
///
/// # Errors
///
/// As [`frame::read`]: [`Error::TooLarge`] before allocating,
/// [`Error::UnknownFrame`] for a kind this build does not have,
/// [`Error::Truncated`] for a partial frame, and the stream's own failure.
pub(crate) async fn read(input: &mut (impl AsyncRead + Unpin)) -> Result<Option<(Kind, Vec<u8>)>> {
    let Some((tag, body)) = read_tagged(input).await? else {
        return Ok(None);
    };
    let Some(kind) = Kind::from_tag(tag) else {
        return Err(Error::UnknownFrame { tag });
    };
    Ok(Some((kind, body)))
}

/// Read one frame under a raw tag, or `None` on a clean goodbye between frames.
///
/// The peer link's reader: its tags are its own space, so the kind is judged
/// by the caller rather than here.
///
/// # Errors
///
/// As [`read`], less the unknown kind.
pub(crate) async fn read_tagged(
    input: &mut (impl AsyncRead + Unpin),
) -> Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0_u8; 5];
    let mut held = 0;
    while held < header.len() {
        let Some(slot) = header.get_mut(held..) else {
            break;
        };
        let read = input.read(slot).await?;
        if read == 0 {
            // Nothing at all is a clean goodbye; a partial header is not.
            return if held == 0 {
                Ok(None)
            } else {
                Err(Error::Truncated)
            };
        }
        held = held.saturating_add(read);
    }
    let mut body = vec![0_u8; frame::announced(&header)?];
    input
        .read_exact(&mut body)
        .await
        .map_err(|_| Error::Truncated)?;
    Ok(Some((header[0], body)))
}

/// Say hello, and hear one back — [`frame::greet`] without the thread.
///
/// # Errors
///
/// [`Error::NotThisProtocol`] when the greeting is not one, and
/// [`Error::WrongVersion`] when it is one this build does not speak.
pub(crate) async fn greet(
    input: &mut (impl AsyncRead + Unpin),
    out: &mut (impl AsyncWrite + Unpin),
) -> Result<u8> {
    out.write_all(HELLO).await?;
    out.write_all(&[MAJOR, MINOR]).await?;
    out.flush().await?;

    // The magic is judged on its own four bytes first, for the reason
    // `frame::greet` gives: a caller that is not a node owes nothing more.
    let mut magic = [0_u8; 4];
    input
        .read_exact(&mut magic)
        .await
        .map_err(|_| Error::NotThisProtocol)?;
    if &magic != HELLO {
        return Err(Error::NotThisProtocol);
    }
    let mut version = [0_u8; 2];
    input
        .read_exact(&mut version)
        .await
        .map_err(|_| Error::Truncated)?;
    let found = version[0];
    if found != MAJOR {
        return Err(Error::WrongVersion {
            found,
            supported: MAJOR,
        });
    }
    Ok(version[1])
}
