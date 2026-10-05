//! The backup's bytes: its head, its frames, and reading exactly as many as asked.

use super::{
    Error, FORMAT, FRAME_RECORD, FRAME_SECTION, FRAME_SHARD_SECTION, Filled, LEGACY_MAGIC, LogSpan,
    MAGIC, check, log_in, shard_log_in,
};
use crate::Result;
use std::io::Read;
use tessari_encoding::NodeVersion;
use tessari_types::Sequence;

/// What a backup file begins with, before any section.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Head {
    /// The build that wrote the file.
    pub(crate) writer: NodeVersion,
    /// How many sections — that is, how many logs — the file holds.
    pub(crate) sections: u32,
}

impl Head {
    /// Read and check it, refusing anything this build cannot read **before**
    /// any record is looked at.
    pub(crate) fn read(input: &mut impl Read) -> Result<Self> {
        Self::magic(input)?;
        let mut versions = [0_u8; 2];
        input
            .read_exact(&mut versions)
            .map_err(|_| Error::NotABackup)?;
        let format = versions.first().copied().unwrap_or(0);
        if format != FORMAT {
            return Err(Error::Unsupported {
                what: "format",
                found: format,
                supported: FORMAT,
            });
        }
        let codec = versions.get(1).copied().unwrap_or(0);
        if codec != tessari_encoding::CODEC_VERSION {
            return Err(Error::Unsupported {
                what: "record codec",
                found: codec,
                supported: tessari_encoding::CODEC_VERSION,
            });
        }
        let writer = Self::writer(input)?;
        // Newer is refused; older is not. The asymmetry is the point — see
        // `Error::WrittenByNewer`.
        let running = NodeVersion::current();
        if writer > running {
            return Err(Error::WrittenByNewer {
                found: writer.to_string(),
                supported: running.to_string(),
            });
        }
        let mut counted = [0_u8; 4];
        input
            .read_exact(&mut counted)
            .map_err(|_| Error::NotABackup)?;
        Ok(Self {
            writer,
            sections: u32::from_be_bytes(counted),
        })
    }

    /// Read the name the file begins with, accepting the one older files carry.
    ///
    /// The old name dropped two letters out of the product's own stem and was
    /// corrected, which moved the bytes a new file starts with. A file written
    /// before that is still a backup, and this header's whole asymmetry —
    /// a newer writer refused, an older one welcomed — says that older files
    /// are the ordinary case. Refusing one over a rename would contradict that,
    /// and it would do it by reporting `NotABackup`, which is not true of the
    /// file in hand.
    pub(crate) fn magic(input: &mut impl Read) -> Result<()> {
        let mut opening = [0_u8; 8];
        input
            .read_exact(&mut opening)
            .map_err(|_| Error::NotABackup)?;
        if &opening == LEGACY_MAGIC {
            return Ok(());
        }
        let (lead, rest) = MAGIC.split_at(opening.len());
        if opening != *lead {
            return Err(Error::NotABackup);
        }
        let mut trailing = [0_u8; 2];
        input
            .read_exact(&mut trailing)
            .map_err(|_| Error::NotABackup)?;
        if trailing != *rest {
            return Err(Error::NotABackup);
        }
        Ok(())
    }

    /// The three numbers naming the build that wrote the file.
    pub(crate) fn writer(input: &mut impl Read) -> Result<NodeVersion> {
        let mut raw = [0_u8; 12];
        input.read_exact(&mut raw).map_err(|_| Error::NotABackup)?;
        let (major, rest) = raw.split_at(4);
        let (minor, patch) = rest.split_at(4);
        Ok(NodeVersion {
            major: u32::from_be_bytes(major.try_into().map_err(|_| Error::NotABackup)?),
            minor: u32::from_be_bytes(minor.try_into().map_err(|_| Error::NotABackup)?),
            patch: u32::from_be_bytes(patch.try_into().map_err(|_| Error::NotABackup)?),
        })
    }
}

/// One frame of a backup file.
#[derive(Debug)]
pub(crate) enum Frame {
    /// A section opening: the log that follows, and the range of it held.
    Section(LogSpan),
    /// One log record, belonging to the section above it.
    Record {
        /// Where in that log it was.
        sequence: Sequence,
        /// Its encoded bytes, checksum already agreed.
        body: Vec<u8>,
    },
    /// The file ended inside a frame.
    Cut,
}

impl Frame {
    /// The next frame, or `None` at a clean end of the file.
    pub(crate) fn next(input: &mut impl Read) -> Result<Option<Self>> {
        let mut tag = [0_u8; 1];
        match fill(input, &mut tag)? {
            Filled::Empty => return Ok(None),
            Filled::Short | Filled::Whole => {}
        }
        match tag.first().copied().unwrap_or(0) {
            FRAME_SECTION => Self::section(input),
            FRAME_SHARD_SECTION => Self::shard_section(input),
            FRAME_RECORD => Self::record(input),
            // Not a truncation and not a guess: the framing is self-describing,
            // so a tag this build does not know is a file it cannot read rather
            // than one it should skip past.
            _ => Err(Error::NotABackup),
        }
    }

    /// A section frame: the log, then the bounds that count inside it.
    pub(crate) fn section(input: &mut impl Read) -> Result<Option<Self>> {
        let mut raw = [0_u8; 41];
        if !matches!(fill(input, &mut raw)?, Filled::Whole) {
            return Ok(Some(Self::Cut));
        }
        let (named, bounds) = raw.split_at(25);
        let named: [u8; 25] = named.try_into().map_err(|_| Error::NotABackup)?;
        let log = log_in(named).ok_or(Error::NotABackup)?;
        let (from, tail) = bounds.split_at(8);
        Ok(Some(Self::Section(LogSpan {
            log,
            from: Sequence::new(u64::from_be_bytes(
                from.try_into().map_err(|_| Error::NotABackup)?,
            )),
            tail: Sequence::new(u64::from_be_bytes(
                tail.try_into().map_err(|_| Error::NotABackup)?,
            )),
        })))
    }

    /// A shard section frame: the shard's log, then the same bounds.
    pub(crate) fn shard_section(input: &mut impl Read) -> Result<Option<Self>> {
        let mut raw = [0_u8; 49];
        if !matches!(fill(input, &mut raw)?, Filled::Whole) {
            return Ok(Some(Self::Cut));
        }
        let (named, bounds) = raw.split_at(33);
        let named: [u8; 33] = named.try_into().map_err(|_| Error::NotABackup)?;
        let log = shard_log_in(named).ok_or(Error::NotABackup)?;
        let (from, tail) = bounds.split_at(8);
        Ok(Some(Self::Section(LogSpan {
            log,
            from: Sequence::new(u64::from_be_bytes(
                from.try_into().map_err(|_| Error::NotABackup)?,
            )),
            tail: Sequence::new(u64::from_be_bytes(
                tail.try_into().map_err(|_| Error::NotABackup)?,
            )),
        })))
    }

    /// A record frame: its length, its sequence, its checksum, its bytes.
    pub(crate) fn record(input: &mut impl Read) -> Result<Option<Self>> {
        let mut header = [0_u8; 16];
        if !matches!(fill(input, &mut header)?, Filled::Whole) {
            return Ok(Some(Self::Cut));
        }
        let (framing, rest) = header.split_at(4);
        let (numbered, checked) = rest.split_at(8);
        let length = usize::try_from(u32::from_be_bytes(
            framing.try_into().map_err(|_| Error::NotABackup)?,
        ))
        .unwrap_or(0);
        let sequence = Sequence::new(u64::from_be_bytes(
            numbered.try_into().map_err(|_| Error::NotABackup)?,
        ));
        let expected = u32::from_be_bytes(checked.try_into().map_err(|_| Error::NotABackup)?);

        // Read as it arrives rather than allocated as declared: the length is the
        // file's claim, and a file that claims 4 GiB in 190 bytes must cost what
        // it holds, not what it says (G061, found by fuzzing).
        let mut body = Vec::new();
        let wanted = u64::try_from(length).unwrap_or(u64::MAX);
        let arrived = input.by_ref().take(wanted).read_to_end(&mut body)?;
        if arrived < length {
            return Ok(Some(Self::Cut));
        }
        if check::crc32(&body) != expected {
            return Err(Error::Damaged {
                sequence: sequence.get(),
            });
        }
        Ok(Some(Self::Record { sequence, body }))
    }
}

/// Read exactly as much as `into` holds, distinguishing a clean end from a cut.
///
/// `read_exact` collapses the two, and they mean different things: a file that
/// ends between records is complete, and one that ends inside a record is not.
pub(crate) fn fill(input: &mut impl Read, into: &mut [u8]) -> std::io::Result<Filled> {
    let mut held = 0;
    while held < into.len() {
        let Some(slot) = into.get_mut(held..) else {
            break;
        };
        let read = input.read(slot)?;
        if read == 0 {
            return Ok(if held == 0 {
                Filled::Empty
            } else {
                Filled::Short
            });
        }
        held = held.saturating_add(read);
    }
    Ok(Filled::Whole)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::Frame;

    /// A reader holding a few bytes that refuses to be handed a buffer far
    /// larger than anything it could fill — the shape of a reader allocating
    /// what a file *says* rather than what it holds.
    struct Short<'a> {
        held: &'a [u8],
    }

    impl std::io::Read for Short<'_> {
        fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
            assert!(
                into.len() <= 1 << 20,
                "handed a {} byte buffer for {} bytes that exist",
                into.len(),
                self.held.len()
            );
            let taken = into.len().min(self.held.len());
            let (now, rest) = self.held.split_at(taken);
            into.get_mut(..taken)
                .unwrap_or_default()
                .copy_from_slice(now);
            self.held = rest;
            Ok(taken)
        }
    }

    #[test]
    fn a_record_claiming_more_than_the_file_holds_is_a_cut_and_allocates_what_arrived() {
        // Found by fuzzing (G061 C3): a 190-byte file whose record frame claimed
        // 4 GiB made `--verify` allocate it before reading a byte of it.
        let mut frame = Vec::new();
        frame.extend_from_slice(&u32::MAX.to_be_bytes());
        frame.extend_from_slice(&7_u64.to_be_bytes());
        frame.extend_from_slice(&0_u32.to_be_bytes());
        frame.extend_from_slice(b"only these bytes");
        let read = Frame::record(&mut Short { held: &frame });
        assert!(matches!(read, Ok(Some(Frame::Cut))), "{read:?}");
    }
}
