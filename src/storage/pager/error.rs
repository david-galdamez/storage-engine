//! Typed errors for the pager.

use crate::storage::pager::types::PageId;

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
