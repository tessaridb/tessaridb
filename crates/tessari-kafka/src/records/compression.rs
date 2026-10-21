//! The four codecs a producer may compress a batch with, read back.
//!
//! Only decompression lives here. A fetch answers uncompressed batches, which
//! every client reads whatever it produced with, so the node never spends CPU
//! compressing on a consumer's behalf.
//!
//! Every decoder works under one ceiling, [`DECOMPRESSED_CEILING`]: a
//! compressed payload is a number of bytes a client chose, and a few kilobytes
//! of zeros can claim gigabytes.

use crate::error::{Error, Result};

/// The most bytes one batch may decompress to.
///
/// Four times the frame ceiling. A batch already passed the frame's sixteen
/// mebibytes compressed, and text compresses well — but not, honestly, past
/// four to one at the sizes a producer batches.
pub const DECOMPRESSED_CEILING: usize = 64 * 1024 * 1024;

/// How a batch's records were compressed, from its attribute bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Not compressed.
    None,
    /// gzip (RFC 1952).
    Gzip,
    /// Snappy, raw or in the stream framing Java producers write.
    Snappy,
    /// LZ4 in the frame format.
    Lz4,
    /// Zstandard.
    Zstd,
}

impl Codec {
    /// The codec the low three attribute bits name.
    ///
    /// # Errors
    ///
    /// [`Error::Codec`] for any other value.
    pub fn of(attributes: i16) -> Result<Self> {
        match attributes & 0x07 {
            0 => Ok(Self::None),
            1 => Ok(Self::Gzip),
            2 => Ok(Self::Snappy),
            3 => Ok(Self::Lz4),
            4 => Ok(Self::Zstd),
            other => Err(Error::Codec {
                codec: u16::try_from(other).unwrap_or(u16::MAX),
            }),
        }
    }

    /// Its name, for a refusal.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gzip => "gzip",
            Self::Snappy => "snappy",
            Self::Lz4 => "lz4",
            Self::Zstd => "zstd",
        }
    }
}

fn refused(codec: Codec, why: impl ToString) -> Error {
    Error::Decompress {
        codec: codec.name(),
        why: why.to_string(),
    }
}

fn too_large(found: usize) -> Error {
    Error::TooLarge {
        field: "decompressed batch",
        found: u64::try_from(found).unwrap_or(u64::MAX),
        ceiling: u64::try_from(DECOMPRESSED_CEILING).unwrap_or(u64::MAX),
    }
}

/// The records of a batch, decompressed.
///
/// # Errors
///
/// [`Error::Decompress`] for a payload the codec cannot read, and
/// [`Error::TooLarge`] for one past [`DECOMPRESSED_CEILING`].
pub fn decompress(codec: Codec, payload: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>> {
    match codec {
        Codec::None => Ok(std::borrow::Cow::Borrowed(payload)),
        Codec::Gzip => gzip(payload).map(std::borrow::Cow::Owned),
        Codec::Snappy => snappy(payload).map(std::borrow::Cow::Owned),
        Codec::Lz4 => lz4(payload).map(std::borrow::Cow::Owned),
        Codec::Zstd => zstd(payload).map(std::borrow::Cow::Owned),
    }
}

fn gzip(payload: &[u8]) -> Result<Vec<u8>> {
    let codec = Codec::Gzip;
    let header = payload
        .get(..10)
        .ok_or_else(|| refused(codec, "shorter than a header"))?;
    if header.get(..3) != Some(&[0x1f, 0x8b, 0x08][..]) {
        return Err(refused(codec, "not a deflate gzip member"));
    }
    let flags = header.get(3).copied().unwrap_or(0);
    let mut at = 10_usize;
    let skip = |at: usize, count: usize| {
        at.checked_add(count)
            .ok_or_else(|| refused(codec, "a header past its end"))
    };
    if flags & 0x04 != 0 {
        let extra = payload
            .get(at..skip(at, 2)?)
            .ok_or_else(|| refused(codec, "a truncated extra field"))?;
        let length = usize::from(u16::from_le_bytes([extra[0], extra[1]]));
        at = skip(skip(at, 2)?, length)?;
    }
    for flag in [0x08_u8, 0x10] {
        if flags & flag != 0 {
            let end = payload
                .get(at..)
                .and_then(|rest| rest.iter().position(|byte| *byte == 0))
                .ok_or_else(|| refused(codec, "an unterminated name or comment"))?;
            at = skip(skip(at, end)?, 1)?;
        }
    }
    if flags & 0x02 != 0 {
        at = skip(at, 2)?;
    }
    let body_end = payload
        .len()
        .checked_sub(8)
        .filter(|end| *end >= at)
        .ok_or_else(|| refused(codec, "no room for the trailer"))?;
    let deflated = payload
        .get(at..body_end)
        .ok_or_else(|| refused(codec, "a truncated body"))?;
    let inflated =
        miniz_oxide::inflate::decompress_to_vec_with_limit(deflated, DECOMPRESSED_CEILING)
            .map_err(|failure| match failure.status {
                miniz_oxide::inflate::TINFLStatus::HasMoreOutput => too_large(DECOMPRESSED_CEILING),
                _ => refused(codec, format!("{:?}", failure.status)),
            })?;
    let trailer = payload
        .get(body_end..)
        .ok_or_else(|| refused(codec, "a truncated trailer"))?;
    let stored = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    if crate::records::crc32::ieee(&inflated) != stored {
        return Err(refused(codec, "the trailer's checksum does not match"));
    }
    let wrapped = u64::try_from(inflated.len()).unwrap_or(u64::MAX) & 0xffff_ffff;
    if u32::try_from(wrapped).ok() != Some(size) {
        return Err(refused(codec, "the trailer's size does not match"));
    }
    Ok(inflated)
}

/// The stream framing Java producers write: a magic, two version words, then
/// blocks each prefixed with its big-endian length.
const SNAPPY_STREAM_MAGIC: &[u8] = b"\x82SNAPPY\x00";

fn snappy(payload: &[u8]) -> Result<Vec<u8>> {
    let codec = Codec::Snappy;
    let mut decoder = snap::raw::Decoder::new();
    let mut block = |input: &[u8], out: &mut Vec<u8>| -> Result<()> {
        let length = snap::raw::decompress_len(input).map_err(|why| refused(codec, why))?;
        let total = out
            .len()
            .checked_add(length)
            .ok_or_else(|| too_large(usize::MAX))?;
        if total > DECOMPRESSED_CEILING {
            return Err(too_large(total));
        }
        out.extend_from_slice(
            &decoder
                .decompress_vec(input)
                .map_err(|why| refused(codec, why))?,
        );
        Ok(())
    };
    let mut out = Vec::new();
    if !payload.starts_with(SNAPPY_STREAM_MAGIC) {
        block(payload, &mut out)?;
        return Ok(out);
    }
    let mut at = SNAPPY_STREAM_MAGIC
        .len()
        .checked_add(8)
        .ok_or_else(|| refused(codec, "header"))?;
    while at < payload.len() {
        let size_end = at
            .checked_add(4)
            .ok_or_else(|| refused(codec, "a block past its end"))?;
        let size = payload
            .get(at..size_end)
            .ok_or_else(|| refused(codec, "a truncated block length"))?;
        let size = usize::try_from(u32::from_be_bytes([size[0], size[1], size[2], size[3]]))
            .map_err(|_| refused(codec, "a block length"))?;
        let end = size_end
            .checked_add(size)
            .ok_or_else(|| refused(codec, "a block past its end"))?;
        let input = payload
            .get(size_end..end)
            .ok_or_else(|| refused(codec, "a truncated block"))?;
        block(input, &mut out)?;
        at = end;
    }
    Ok(out)
}

/// The LZ4 frame magic.
const LZ4_MAGIC: u32 = 0x184D_2204;

fn lz4(payload: &[u8]) -> Result<Vec<u8>> {
    let codec = Codec::Lz4;
    let take = |at: usize, count: usize| -> Result<(&[u8], usize)> {
        let end = at
            .checked_add(count)
            .ok_or_else(|| refused(codec, "a field past its end"))?;
        let bytes = payload
            .get(at..end)
            .ok_or_else(|| refused(codec, "truncated"))?;
        Ok((bytes, end))
    };
    let (magic, mut at) = take(0, 4)?;
    if u32::from_le_bytes([magic[0], magic[1], magic[2], magic[3]]) != LZ4_MAGIC {
        return Err(refused(codec, "not an LZ4 frame"));
    }
    let (descriptor, next) = take(at, 2)?;
    at = next;
    let (flags, block_descriptor) = (descriptor[0], descriptor[1]);
    if flags >> 6 != 0b01 {
        return Err(refused(codec, "an LZ4 frame version other than 1"));
    }
    let block_checksum = flags & 0x10 != 0;
    let content_size = flags & 0x08 != 0;
    let content_checksum = flags & 0x04 != 0;
    let dictionary = flags & 0x01 != 0;
    let block_max = match (block_descriptor >> 4) & 0x07 {
        4 => 64 * 1024,
        5 => 256 * 1024,
        6 => 1024 * 1024,
        7 => 4 * 1024 * 1024,
        _ => return Err(refused(codec, "an unknown block size")),
    };
    if content_size {
        at = take(at, 8)?.1;
    }
    if dictionary {
        return Err(refused(codec, "a frame that needs a dictionary"));
    }
    at = take(at, 1)?.1;
    let mut out = Vec::new();
    let mut buffer = vec![0_u8; block_max];
    loop {
        let (size, next) = take(at, 4)?;
        at = next;
        let word = u32::from_le_bytes([size[0], size[1], size[2], size[3]]);
        if word == 0 {
            break;
        }
        let stored = word & 0x8000_0000 != 0;
        let length =
            usize::try_from(word & 0x7fff_ffff).map_err(|_| refused(codec, "a block size"))?;
        if length > block_max {
            return Err(refused(codec, "a block larger than the frame's block size"));
        }
        let (block, next) = take(at, length)?;
        at = next;
        if block_checksum {
            at = take(at, 4)?.1;
        }
        let total = out
            .len()
            .checked_add(block_max)
            .ok_or_else(|| too_large(usize::MAX))?;
        if total > DECOMPRESSED_CEILING.saturating_add(block_max) {
            return Err(too_large(total));
        }
        if stored {
            out.extend_from_slice(block);
        } else {
            let written = lz4_flex::block::decompress_into(block, &mut buffer)
                .map_err(|why| refused(codec, why))?;
            out.extend_from_slice(buffer.get(..written).unwrap_or_default());
        }
        if out.len() > DECOMPRESSED_CEILING {
            return Err(too_large(out.len()));
        }
    }
    if content_checksum {
        take(at, 4)?;
    }
    Ok(out)
}

fn zstd(payload: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let codec = Codec::Zstd;
    let decoder =
        ruzstd::decoding::StreamingDecoder::new(payload).map_err(|why| refused(codec, why))?;
    let mut out = Vec::new();
    let limit = u64::try_from(DECOMPRESSED_CEILING)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    decoder
        .take(limit)
        .read_to_end(&mut out)
        .map_err(|why| refused(codec, why))?;
    if out.len() > DECOMPRESSED_CEILING {
        return Err(too_large(out.len()));
    }
    Ok(out)
}
