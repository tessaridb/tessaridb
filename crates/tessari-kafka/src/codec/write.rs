//! Writing the protocol's primitive types into a frame.
//!
//! The mirror of [`crate::codec::Reader`]: the same flexible switch decides how
//! a length is written. Lengths that do not fit their encoding are refused at
//! the call rather than truncated, because a truncated length is a frame the
//! client misreads from that byte on.

use crate::error::{Error, Result};

/// A growing frame body.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// An empty frame.
    #[must_use]
    pub const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// An empty frame with room for `capacity` bytes.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
        }
    }

    /// The bytes written so far.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// How many bytes have been written.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Raw bytes, with no length in front.
    pub fn raw(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Overwrite four bytes already written at `at` — a length or a checksum
    /// known only once what follows it is written.
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`] when `at` is not inside what was written.
    pub fn patch_u32(&mut self, at: usize, value: u32) -> Result<()> {
        let end = at
            .checked_add(4)
            .ok_or(Error::Truncated { field: "patch" })?;
        let slot = self
            .bytes
            .get_mut(at..end)
            .ok_or(Error::Truncated { field: "patch" })?;
        slot.copy_from_slice(&value.to_be_bytes());
        Ok(())
    }

    /// The bytes from `from` to the end.
    #[must_use]
    pub fn since(&self, from: usize) -> &[u8] {
        self.bytes.get(from..).unwrap_or_default()
    }

    /// An `int8`.
    pub fn i8(&mut self, value: i8) {
        self.raw(&value.to_be_bytes());
    }

    /// A `boolean`.
    pub fn bool(&mut self, value: bool) {
        self.i8(i8::from(value));
    }

    /// An `int16`.
    pub fn i16(&mut self, value: i16) {
        self.raw(&value.to_be_bytes());
    }

    /// An `int32`.
    pub fn i32(&mut self, value: i32) {
        self.raw(&value.to_be_bytes());
    }

    /// A `uint32`.
    pub fn u32(&mut self, value: u32) {
        self.raw(&value.to_be_bytes());
    }

    /// An `int64`.
    pub fn i64(&mut self, value: i64) {
        self.raw(&value.to_be_bytes());
    }

    /// A `uuid`.
    pub fn uuid(&mut self, value: [u8; 16]) {
        self.raw(&value);
    }

    /// An unsigned varint.
    pub fn uvarlong(&mut self, mut value: u64) {
        loop {
            let low = u8::try_from(value & 0x7f).unwrap_or(0);
            value = value.wrapping_shr(7);
            if value == 0 {
                self.bytes.push(low);
                return;
            }
            self.bytes.push(low | 0x80);
        }
    }

    /// An unsigned varint of 32 bits.
    pub fn uvarint(&mut self, value: u32) {
        self.uvarlong(u64::from(value));
    }

    /// A zig-zag `varint`.
    pub fn varint(&mut self, value: i32) {
        let zigzag = value.wrapping_shl(1) ^ value.wrapping_shr(31);
        self.uvarint(u32::from_ne_bytes(zigzag.to_ne_bytes()));
    }

    /// A zig-zag `varlong`.
    pub fn varlong(&mut self, value: i64) {
        let zigzag = value.wrapping_shl(1) ^ value.wrapping_shr(63);
        self.uvarlong(u64::from_ne_bytes(zigzag.to_ne_bytes()));
    }

    fn length(&mut self, flexible: bool, wide: bool, length: Option<usize>) -> Result<()> {
        let field = "length";
        if flexible {
            let encoded = match length {
                None => 0,
                Some(length) => u32::try_from(length)
                    .ok()
                    .and_then(|length| length.checked_add(1))
                    .ok_or(Error::TooLarge {
                        field,
                        found: u64::try_from(length).unwrap_or(u64::MAX),
                        ceiling: u64::from(u32::MAX),
                    })?,
            };
            self.uvarint(encoded);
        } else if wide {
            let encoded = match length {
                None => -1,
                Some(length) => i32::try_from(length).map_err(|_| Error::TooLarge {
                    field,
                    found: u64::try_from(length).unwrap_or(u64::MAX),
                    ceiling: u64::from(i32::MAX.unsigned_abs()),
                })?,
            };
            self.i32(encoded);
        } else {
            let encoded = match length {
                None => -1,
                Some(length) => i16::try_from(length).map_err(|_| Error::TooLarge {
                    field,
                    found: u64::try_from(length).unwrap_or(u64::MAX),
                    ceiling: u64::from(i16::MAX.unsigned_abs()),
                })?,
            };
            self.i16(encoded);
        }
        Ok(())
    }

    /// A nullable string.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] past the encoding's length.
    pub fn nullable_string(&mut self, flexible: bool, value: Option<&str>) -> Result<()> {
        self.length(flexible, false, value.map(str::len))?;
        if let Some(value) = value {
            self.raw(value.as_bytes());
        }
        Ok(())
    }

    /// A string.
    ///
    /// # Errors
    ///
    /// As [`Writer::nullable_string`].
    pub fn string(&mut self, flexible: bool, value: &str) -> Result<()> {
        self.nullable_string(flexible, Some(value))
    }

    /// Nullable bytes.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] past the encoding's length.
    pub fn nullable_bytes(&mut self, flexible: bool, value: Option<&[u8]>) -> Result<()> {
        self.length(flexible, true, value.map(<[u8]>::len))?;
        if let Some(value) = value {
            self.raw(value);
        }
        Ok(())
    }

    /// Bytes.
    ///
    /// # Errors
    ///
    /// As [`Writer::nullable_bytes`].
    pub fn bytes(&mut self, flexible: bool, value: &[u8]) -> Result<()> {
        self.nullable_bytes(flexible, Some(value))
    }

    /// An array's element count, `None` for a null array.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] past the encoding's count.
    pub fn array_len(&mut self, flexible: bool, count: Option<usize>) -> Result<()> {
        self.length(flexible, true, count)
    }

    /// An empty tagged-field section.
    pub fn no_tagged_fields(&mut self) {
        self.uvarint(0);
    }
}
