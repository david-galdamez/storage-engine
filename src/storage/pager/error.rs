//! Typed errors for the pager.

use crate::storage::pager::{page::PageType, types::PageId};

/// Errors from encoding, decoding, and validating page contents in memory (no I/O).
#[derive(Debug, Clone, Copy, thiserror::Error, PartialEq, Eq)]
pub enum PageError {
    /// An access of `len` bytes at `offset` does not fit in a buffer of `buf_len` bytes.
    #[error("Out of bounds error: offset: {offset}, len: {len}, buf_len: {buf_len}")]
    OutOfBounds {
        /// Byte offset where the access starts.
        offset: usize,
        /// Number of bytes accessed.
        len: usize,
        /// Length of the buffer in bytes.
        buf_len: usize,
    },
    /// The page size is not a power of two in `[512, 65536]`. Field `size` is the rejected value.
    #[error("Invalid size: {size}")]
    InvalidPageSize { size: u32 },

    /// The page type byte is not a known [`PageType`]. Field `value` is the byte read.
    #[error("Unknown page type: {value}")]
    UnknownPageType { value: u8 },

    /// The page has a valid type, but not the one the caller needs.
    #[error("Unexpected page type, expected: {expected} - found: {found}")]
    UnexpectedPageType { expected: PageType, found: PageType },

    /// The stored checksum does not match the page contents. Field `stored` is
    /// the value at offset 0; field `computed` is the CRC32 of `[4, len)`.
    #[error("Checksum mismatch, stored: {stored:#010x} - computed: {computed:#010x}")]
    ChecksumMismatch { stored: u32, computed: u32 },

    /// Page 0 does not start with the database magic number, so the file is not
    /// a database of this engine (or its first page is destroyed). Field `found`
    /// holds the 8 bytes read instead of the magic.
    #[error("Bad magic: expected \"STENGDB\\0\", found {found:02x?}")]
    BadMagic { found: [u8; 8] },

    /// The file was written with a format version this build cannot read.
    /// Field `version` is the version stored in the file header.
    #[error("Unsupported format version: {version}")]
    UnsupportedVersion { version: u16 },

    /// The file header fields contradict each other (ADR 0003 rules), for
    /// example a free list head past the end of the file. Field `reason` names
    /// the rule that failed.
    #[error("Corrupt file header: {reason}")]
    CorruptHeader { reason: &'static str },
}

/// Errors from the file-backed pager.
#[derive(Debug, thiserror::Error)]
pub enum PagerError {
    /// An operating-system I/O error.
    #[error("Io error: {0}")]
    Io(#[from] std::io::Error),
    /// A page or header could not be decoded or encoded.
    #[error("Format error: {0}")]
    Format(#[from] PageError),
    /// A page failed validation. Field `page` is the page that failed; field `source` is the underlying page error.
    #[error("Corrupted error, page_id: {page}, source: {source}")]
    Corrupted { page: PageId, source: PageError },
}
