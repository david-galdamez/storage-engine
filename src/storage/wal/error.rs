//! Typed errors for the WAL codec and file.

use crate::storage::pager::error::PageError;

/// Errors from the WAL header, records and file.
///
/// A torn or corrupt record in the log is not an error: the reader treats it as
/// the end of the log (ADR 0006).
#[derive(Debug, thiserror::Error)]
pub enum WalError {
    /// The WAL header is too short or its CRC32 does not match.
    #[error("corrupt WAL header")]
    CorruptHeader,

    /// The first 8 bytes are not the WAL magic number.
    #[error("bad WAL magic: {found:?}")]
    BadMagic {
        /// The bytes found where the magic should be.
        found: [u8; 8],
    },

    /// The WAL version is not one this build can read.
    #[error("unsupported WAL version: {version}")]
    UnsupportedVersion {
        /// The version found in the header.
        version: u16,
    },

    /// The WAL page size differs from the database page size.
    #[error("WAL page size {wal} does not match database page size {database}")]
    PageSizeMismatch {
        /// Page size stored in the WAL header.
        wal: u32,
        /// Page size of the database file.
        database: u32,
    },

    /// The record would not fit in the `u32` `total_len` field.
    #[error("WAL record too large")]
    RecordTooLarge,

    /// A record must hold at least one page image.
    #[error("WAL record has no page images")]
    EmptyRecord,

    /// A page image does not have exactly `page_size` bytes.
    #[error("page image has {found} bytes, expected {expected}")]
    ImageLength {
        /// The page size of the WAL.
        expected: usize,
        /// The length of the rejected image.
        found: usize,
    },

    /// A low-level codec failure (buffer too short, invalid page size).
    #[error(transparent)]
    Page(#[from] PageError),

    /// An I/O error on the WAL file.
    #[error("WAL I/O error: {0}")]
    Io(#[from] std::io::Error),
}
