//! The same frames as `frame.rs`, read and written without holding a thread.
//!
//! Only the node reads asynchronously; a client and the peer link keep the
//! blocking half. The header's encoding and the ceiling are not repeated here —
//! [`frame::header`] and [`frame::announced`] are the one copy of each, so the
//! two halves cannot come to disagree about what a frame is.

use std::time::Duration;

use tessari_constants::FRAME_STALL_SECONDS;
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
        // Waiting for a frame to begin has no deadline — a quiet connection is a
        // pooled session or a subscriber doing its job. Once one has begun, every
        // further byte must come within the stall (R-01).
        let read = if held == 0 {
            input.read(slot).await?
        } else {
            within_the_stall(input.read(slot)).await?
        };
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
    let mut filled = 0;
    while filled < body.len() {
        let Some(slot) = body.get_mut(filled..) else {
            break;
        };
        let read = within_the_stall(input.read(slot))
            .await
            .map_err(|failure| match failure {
                Error::Stalled => Error::Stalled,
                _ => Error::Truncated,
            })?;
        if read == 0 {
            return Err(Error::Truncated);
        }
        filled = filled.saturating_add(read);
    }
    Ok(Some((header[0], body)))
}

/// One read inside a frame, refused as [`Error::Stalled`] when it does not
/// complete within [`FRAME_STALL_SECONDS`].
async fn within_the_stall(
    read: impl std::future::Future<Output = std::io::Result<usize>>,
) -> Result<usize> {
    tokio::time::timeout(Duration::from_secs(FRAME_STALL_SECONDS), read)
        .await
        .map_err(|_| Error::Stalled)?
        .map_err(Error::from)
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tessari_constants::FRAME_STALL_SECONDS;
    use tokio::io::AsyncWriteExt as _;

    use super::read_tagged;
    use crate::error::Error;
    use crate::frame;

    #[tokio::test(start_paused = true)]
    async fn a_frame_that_stops_halfway_is_given_up_on_after_the_stall() {
        let (mut ours, mut theirs) = tokio::io::duplex(64);
        // A header announcing eight bytes, and only three of them.
        theirs
            .write_all(&frame::header(1, &[0; 8]).expect("a header"))
            .await
            .expect("written");
        theirs.write_all(&[1, 2, 3]).await.expect("written");
        let reading = tokio::spawn(async move { read_tagged(&mut ours).await });
        tokio::time::advance(Duration::from_secs(FRAME_STALL_SECONDS + 1)).await;
        let read = reading.await.expect("written");
        assert!(matches!(read, Err(Error::Stalled)), "{read:?}");
        drop(theirs);
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_quiet_between_frames_is_never_given_up_on() {
        let (mut ours, mut theirs) = tokio::io::duplex(64);
        let reading = tokio::spawn(async move { read_tagged(&mut ours).await });
        // An hour of nothing, which is a pooled session doing its job.
        tokio::time::advance(Duration::from_secs(3600)).await;
        theirs
            .write_all(&frame::header(1, &[7]).expect("a header"))
            .await
            .expect("written");
        theirs.write_all(&[7]).await.expect("written");
        let read = reading
            .await
            .expect("held")
            .expect("a frame")
            .expect("not a goodbye");
        assert_eq!(read, (1, vec![7]));
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_frame_that_keeps_arriving_is_read_whole() {
        let (mut ours, mut theirs) = tokio::io::duplex(64);
        let reading = tokio::spawn(async move { read_tagged(&mut ours).await });
        theirs
            .write_all(&frame::header(1, &[0; 4]).expect("a header"))
            .await
            .expect("written");
        for byte in 0..4_u8 {
            // Each byte inside the stall, the whole frame well past it.
            tokio::time::advance(Duration::from_secs(FRAME_STALL_SECONDS - 1)).await;
            theirs.write_all(&[byte]).await.expect("written");
        }
        let read = reading
            .await
            .expect("held")
            .expect("a frame")
            .expect("not a goodbye");
        assert_eq!(read, (1, vec![0, 1, 2, 3]));
    }
}
