//! Typed errors for the B+Tree node views.

use crate::storage::pager::error::PageError;

/// Errors from reading and writing B+Tree nodes in memory (no I/O).
#[derive(Debug, Clone, Copy, thiserror::Error, PartialEq, Eq)]
pub enum NodeError {
    /// The underlying page is wrong or an operation on the slotted page failed,
    /// for example the page has the wrong type or is full.
    #[error("Page error: {0}")]
    Page(#[from] PageError),

    /// A key is longer than `max_key_len` for the page size. Field `len` is the
    /// rejected length; field `max` is the limit.
    #[error("Key too large, len: {len} - max: {max}")]
    KeyTooLarge { len: usize, max: usize },

    /// A cell is longer than `max_node_cell_len` for the page size. For a leaf
    /// this means the value is too large to be inline: the caller should have
    /// used an overflow chain. Field `len` is the encoded cell length; field
    /// `max` is the limit.
    #[error("Cell too large for a node, len: {len} - max: {max}")]
    CellTooLarge { len: usize, max: usize },

    /// The node contents break a rule of ADR 0005, for example a child pointer
    /// of 0 or keys out of order. A zero child passed to a write operation is
    /// reported the same way. Field `reason` names the rule that failed.
    #[error("Corrupt node: {reason}")]
    CorruptNode { reason: &'static str },
}
