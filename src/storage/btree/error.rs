//! Typed errors for the B+Tree: node views and tree operations.

use crate::storage::pager::{
    error::{PageError, PagerError},
    types::PageId,
};

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

/// Errors from the B+Tree: node errors plus the I/O of the pager under it.
#[derive(Debug, thiserror::Error)]
pub enum BTreeError {
    /// The pager failed: an I/O error, a checksum mismatch or a bad page id.
    #[error("Pager error: {0}")]
    Pager(#[from] PagerError),

    /// A node page failed to decode or validate. Field `page` is the page that
    /// failed; field `source` is the underlying node error.
    #[error("Corrupt node, page_id: {page}, source: {source}")]
    Node { page: PageId, source: NodeError },

    /// `BTree::open` was given a page that cannot be a tree root: page 0, a
    /// page past the end of the file, or a page that is not a B+Tree node.
    /// Field `page` is the rejected page; field `reason` names the rule that
    /// failed.
    #[error("Invalid root, page_id: {page}, reason: {reason}")]
    InvalidRoot { page: PageId, reason: &'static str },
}
