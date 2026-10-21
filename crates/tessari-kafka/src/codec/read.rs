//! Reading the protocol's primitive types out of a frame.
//!
//! One reader serves both encodings a version may use. The protocol's
//! "flexible" versions change how strings, byte strings and arrays carry their
//! length (an unsigned varint holding length + 1, so 0 is null) and add tagged
//! fields at the end of every structure; the older versions use fixed-width
//! signed lengths. A message reads its fields through [`Reader::string`],
//! [`Reader::bytes`] and [`Reader::array_len`] with the flexibility of its
//! version, so each message is written once rather than twice.

use crate::error::{Error, Result};

/// The most elements an array may claim.
///
/// A count is checked against what is left of the frame too (every element is
/// at least one byte), so this ceiling only bites on a frame that is itself
/// enormous.
const ARRAY_CEILING: u64 = 1_000_000;

/// A cursor over one frame.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    /// A reader at the start of `bytes`.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// How many bytes are left.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    /// The bytes not yet read.
    #[must_use]
    pub fn rest(&self) -> &'a [u8] {
        self.bytes.get(self.at..).unwrap_or_default()
    }

    /// The next `count` bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`] when fewer are left.
    pub fn take(&mut self, count: usize, field: &'static str) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(count)
            .ok_or(Error::Truncated { field })?;
        let taken = self
            .bytes
            .get(self.at..end)
            .ok_or(Error::Truncated { field })?;
        self.at = end;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self, field: &'static str) -> Result<[u8; N]> {
        let taken = self.take(N, field)?;
        let mut out = [0_u8; N];
        out.copy_from_slice(taken);
        Ok(out)
    }

    /// An `int8`.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`].
    pub fn i8(&mut self, field: &'static str) -> Result<i8> {
        Ok(i8::from_be_bytes(self.array(field)?))
    }

    /// A `boolean`.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`].
    pub fn bool(&mut self, field: &'static str) -> Result<bool> {
        Ok(self.i8(field)? != 0)
    }

    /// An `int16`.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`].
    pub fn i16(&mut self, field: &'static str) -> Result<i16> {
        Ok(i16::from_be_bytes(self.array(field)?))
    }

    /// An `int32`.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`].
    pub fn i32(&mut self, field: &'static str) -> Result<i32> {
        Ok(i32::from_be_bytes(self.array(field)?))
    }

    /// A `uint32`.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`].
    pub fn u32(&mut self, field: &'static str) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array(field)?))
    }

    /// An `int64`.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`].
    pub fn i64(&mut self, field: &'static str) -> Result<i64> {
        Ok(i64::from_be_bytes(self.array(field)?))
    }

    /// A `uuid`, as its sixteen bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`].
    pub fn uuid(&mut self, field: &'static str) -> Result<[u8; 16]> {
        self.array(field)
    }

    /// An unsigned varint of at most 32 bits.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`], or [`Error::Malformed`] past five bytes.
    pub fn uvarint(&mut self, field: &'static str) -> Result<u32> {
        let wide = self.uvarlong(field)?;
        u32::try_from(wide).map_err(|_| Error::Malformed {
            field,
            why: "a varint wider than 32 bits",
        })
    }

    /// An unsigned varint of at most 64 bits.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`], or [`Error::Malformed`] past ten bytes.
    pub fn uvarlong(&mut self, field: &'static str) -> Result<u64> {
        let mut value: u64 = 0;
        for shift in (0_u32..70).step_by(7) {
            let byte = self.array::<1>(field)?[0];
            let low = u64::from(byte & 0x7f);
            let placed = low.checked_shl(shift).ok_or(Error::Malformed {
                field,
                why: "a varint longer than ten bytes",
            })?;
            if shift == 63 && low > 1 {
                return Err(Error::Malformed {
                    field,
                    why: "a varint wider than 64 bits",
                });
            }
            value |= placed;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::Malformed {
            field,
            why: "a varint longer than ten bytes",
        })
    }

    /// A zig-zag `varint` (signed, at most 32 bits).
    ///
    /// # Errors
    ///
    /// As [`Reader::uvarint`].
    pub fn varint(&mut self, field: &'static str) -> Result<i32> {
        let raw = self.uvarint(field)?;
        let magnitude = i32::try_from(raw.wrapping_shr(1)).map_err(|_| Error::Malformed {
            field,
            why: "a varint out of range",
        })?;
        Ok(if raw & 1 == 0 { magnitude } else { !magnitude })
    }

    /// A zig-zag `varlong` (signed, at most 64 bits).
    ///
    /// # Errors
    ///
    /// As [`Reader::uvarlong`].
    pub fn varlong(&mut self, field: &'static str) -> Result<i64> {
        let raw = self.uvarlong(field)?;
        let magnitude = i64::try_from(raw.wrapping_shr(1)).map_err(|_| Error::Malformed {
            field,
            why: "a varlong out of range",
        })?;
        Ok(if raw & 1 == 0 { magnitude } else { !magnitude })
    }

    /// A length that may be null: `int16` for strings in the classic encoding,
    /// `int32` for bytes, and `uvarint − 1` in a flexible version.
    fn length(&mut self, flexible: bool, wide: bool, field: &'static str) -> Result<Option<usize>> {
        let length: i64 = if flexible {
            i64::from(self.uvarint(field)?).saturating_sub(1)
        } else if wide {
            i64::from(self.i32(field)?)
        } else {
            i64::from(self.i16(field)?)
        };
        if length < 0 {
            return Ok(None);
        }
        let length = usize::try_from(length).map_err(|_| Error::Malformed {
            field,
            why: "a length this platform cannot hold",
        })?;
        if length > self.remaining() {
            return Err(Error::Truncated { field });
        }
        Ok(Some(length))
    }

    /// A nullable string.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`], or [`Error::Malformed`] when it is not UTF-8.
    pub fn nullable_string(
        &mut self,
        flexible: bool,
        field: &'static str,
    ) -> Result<Option<&'a str>> {
        let Some(length) = self.length(flexible, false, field)? else {
            return Ok(None);
        };
        let taken = self.take(length, field)?;
        std::str::from_utf8(taken)
            .map(Some)
            .map_err(|_| Error::Malformed {
                field,
                why: "a string that is not UTF-8",
            })
    }

    /// A string that may not be null.
    ///
    /// # Errors
    ///
    /// As [`Reader::nullable_string`], and [`Error::Malformed`] for a null.
    pub fn string(&mut self, flexible: bool, field: &'static str) -> Result<&'a str> {
        self.nullable_string(flexible, field)?
            .ok_or(Error::Malformed {
                field,
                why: "null where a string is required",
            })
    }

    /// Nullable bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`].
    pub fn nullable_bytes(
        &mut self,
        flexible: bool,
        field: &'static str,
    ) -> Result<Option<&'a [u8]>> {
        let Some(length) = self.length(flexible, true, field)? else {
            return Ok(None);
        };
        self.take(length, field).map(Some)
    }

    /// Bytes that may not be null.
    ///
    /// # Errors
    ///
    /// As [`Reader::nullable_bytes`], and [`Error::Malformed`] for a null.
    pub fn bytes(&mut self, flexible: bool, field: &'static str) -> Result<&'a [u8]> {
        self.nullable_bytes(flexible, field)?
            .ok_or(Error::Malformed {
                field,
                why: "null where bytes are required",
            })
    }

    /// An array's element count, `None` for a null array.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] when the count could not fit in what is left of the
    /// frame or passes the ceiling.
    pub fn array_len(&mut self, flexible: bool, field: &'static str) -> Result<Option<usize>> {
        let count: i64 = if flexible {
            i64::from(self.uvarint(field)?).saturating_sub(1)
        } else {
            i64::from(self.i32(field)?)
        };
        if count < 0 {
            return Ok(None);
        }
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        let room = u64::try_from(self.remaining()).unwrap_or(u64::MAX);
        if count > ARRAY_CEILING || count > room {
            return Err(Error::TooLarge {
                field,
                found: count,
                ceiling: room.min(ARRAY_CEILING),
            });
        }
        usize::try_from(count)
            .map(Some)
            .map_err(|_| Error::Malformed {
                field,
                why: "a count this platform cannot hold",
            })
    }

    /// The tagged fields that close every structure of a flexible version.
    ///
    /// Each tag is skipped, which the protocol requires of a reader that does
    /// not know it: a tag is how a newer client adds a field without a new
    /// version, so refusing an unknown one would refuse the newer client.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`].
    pub fn tagged_fields(&mut self, field: &'static str) -> Result<()> {
        let count = self.uvarint(field)?;
        for _ in 0..count {
            let _tag = self.uvarint(field)?;
            let size = usize::try_from(self.uvarint(field)?).map_err(|_| Error::Malformed {
                field,
                why: "a tag size this platform cannot hold",
            })?;
            self.take(size, field)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
