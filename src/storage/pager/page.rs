//! Raw page buffer and common page header.
//!
//! Every page, including page 0, starts with the same 16 bytes (ADR 0003):
//!
//! | Offset | Size | Field                                   |
//! |--------|------|-----------------------------------------|
//! | 0      | 4    | checksum: u32, CRC32 of `[4, len)`      |
//! | 4      | 1    | page type: u8                           |
//! | 5      | 3    | reserved                                |
//! | 8      | 8    | page LSN: u64 (0 until the WAL exists)  |
//!
//! All integers are little-endian. The functions here are pure (no I/O) and
//! take the whole page as a slice. The checksum covers `[4, buffer.len())`.
//! A buffer shorter than [`COMMON_HEADER_SIZE`] is always an error.

use std::fmt;

use crate::storage::pager::{
    codec::{read_u8, read_u32, read_u64, slice, write_u8, write_u32, write_u64},
    crc32::crc32,
    error::PageError,
};

/// Size in bytes of the header shared by every page.
pub const COMMON_HEADER_SIZE: usize = 16;
const CHECKSUM_OFFSET: usize = 0;
/// First byte covered by the checksum: the checksum protects `[CHECKSUM_END, len)`.
pub const CHECKSUM_END: usize = 4;
const PAGE_TYPE_OFFSET: usize = 4;
const PAGE_LSN_OFFSET: usize = 8;

/// Kind of page, stored at offset 4.
///
/// On disk: 1 = file header, 2 = free, 3 = slotted. Byte 0 is not a variant: it
/// marks an invalid page, so a zero-filled page is never taken for a real one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    /// Page 0: describes the database file.
    FileHeader,
    /// A page in the free list, waiting to be reused.
    Free,
    /// A page holding variable-size cells behind a slot directory.
    Slotted,
}

impl fmt::Display for PageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileHeader => write!(f, "File Header"),
            Self::Free => write!(f, "Free"),
            Self::Slotted => write!(f, "Slotted"),
        }
    }
}

impl TryFrom<u8> for PageType {
    type Error = PageError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::FileHeader),
            2 => Ok(Self::Free),
            3 => Ok(Self::Slotted),
            _ => Err(PageError::UnknownPageType { value }),
        }
    }
}

impl From<PageType> for u8 {
    fn from(value: PageType) -> Self {
        match value {
            PageType::FileHeader => 1,
            PageType::Free => 2,
            PageType::Slotted => 3,
        }
    }
}

const fn check_buffer_size(buffer: &[u8]) -> Result<(), PageError> {
    if buffer.len() < COMMON_HEADER_SIZE {
        return Err(PageError::OutOfBounds {
            offset: 0,
            len: COMMON_HEADER_SIZE,
            buf_len: buffer.len(),
        });
    }

    Ok(())
}

/// Writes the page type at offset 4. Touches no other byte.
///
/// # Errors
///
/// Returns [`PageError::OutOfBounds`] if the buffer is shorter than the
/// common header. The buffer is not modified in that case.
pub fn set_page_type(buffer: &mut [u8], page_type: PageType) -> Result<(), PageError> {
    check_buffer_size(buffer)?;

    let page_type_value = u8::from(page_type);

    write_u8(buffer, PAGE_TYPE_OFFSET, page_type_value)
}

/// Reads the page type at offset 4.
///
/// # Errors
///
/// Returns [`PageError::OutOfBounds`] if the buffer is shorter than the
/// common header, or [`PageError::UnknownPageType`] if the byte is not a
/// known page type.
pub fn page_type(buffer: &[u8]) -> Result<PageType, PageError> {
    check_buffer_size(buffer)?;

    let page_type = read_u8(buffer, PAGE_TYPE_OFFSET)?;

    PageType::try_from(page_type)
}

/// Writes the page LSN at offset 8, little-endian. Touches no other byte.
///
/// # Errors
///
/// Returns [`PageError::OutOfBounds`] if the buffer is shorter than the
/// common header. The buffer is not modified in that case.
pub fn set_page_lsn(buffer: &mut [u8], lsn: u64) -> Result<(), PageError> {
    check_buffer_size(buffer)?;

    write_u64(buffer, PAGE_LSN_OFFSET, lsn)
}

/// Reads the page LSN at offset 8, little-endian.
///
/// # Errors
///
/// Returns [`PageError::OutOfBounds`] if the buffer is shorter than the
/// common header.
pub fn page_lsn(buffer: &[u8]) -> Result<u64, PageError> {
    check_buffer_size(buffer)?;

    read_u64(buffer, PAGE_LSN_OFFSET)
}

/// Computes the CRC32 of bytes `[4, buffer.len())`. Does not read the stored
/// checksum.
///
/// # Errors
///
/// Returns [`PageError::OutOfBounds`] if the buffer is shorter than the
/// common header.
pub fn compute_checksum(buffer: &[u8]) -> Result<u32, PageError> {
    check_buffer_size(buffer)?;

    Ok(crc32(slice(
        buffer,
        CHECKSUM_END,
        buffer.len().saturating_sub(CHECKSUM_END),
    )?))
}

/// Computes the checksum and writes it at offset 0, little-endian.
///
/// Call it last, after every other change to the page: any later change
/// makes [`verify_checksum`] fail.
///
/// # Errors
///
/// Returns [`PageError::OutOfBounds`] if the buffer is shorter than the
/// common header. The buffer is not modified in that case.
pub fn stamp_checksum(buffer: &mut [u8]) -> Result<(), PageError> {
    let checksum = compute_checksum(buffer)?;

    write_u32(buffer, CHECKSUM_OFFSET, checksum)
}

/// Checks that the stored checksum matches the page contents.
///
/// # Errors
///
/// Returns [`PageError::OutOfBounds`] if the buffer is shorter than the
/// common header, or [`PageError::ChecksumMismatch`] with both values if the
/// page is corrupted or was never stamped.
pub fn verify_checksum(buffer: &[u8]) -> Result<(), PageError> {
    let actual_checksum = compute_checksum(buffer)?;

    let last_checksum = read_u32(buffer, CHECKSUM_OFFSET)?;

    if last_checksum != actual_checksum {
        return Err(PageError::ChecksumMismatch {
            stored: last_checksum,
            computed: actual_checksum,
        });
    }

    Ok(())
}

/// Checks that the page has type `page_type`.
///
/// # Errors
///
/// Returns [`PageError::OutOfBounds`] if the buffer is shorter than the
/// common header, [`PageError::UnknownPageType`] if the stored byte is not a
/// known page type, or [`PageError::UnexpectedPageType`] if it is a different
/// known type.
pub fn expect_page_type(buffer: &[u8], expected_page_type: PageType) -> Result<(), PageError> {
    let last_page_type = page_type(buffer)?;

    if last_page_type != expected_page_type {
        return Err(PageError::UnexpectedPageType {
            expected: expected_page_type,
            found: last_page_type,
        });
    }

    Ok(())
}
