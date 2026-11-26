//! Typed errors for the B+Tree: node views and tree operations.

use crate::storage::pager::{
    error::{PageError, PagerError},
    page::PageType,
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

    /// The structure of the tree is wrong, not one node: a child pointer
    /// leads to a page that is not a B+Tree node, or a descent is deeper than
    /// any valid tree, for example because of a cycle. Field `page` is the
    /// page where the problem was found; field `reason` names the rule that
    /// failed.
    #[error("Corrupt tree, page_id: {page}, reason: {reason}")]
    CorruptTree { page: PageId, reason: &'static str },

    /// `put` was given a key longer than `max_key_len` for the page size. This
    /// is a caller error, not corruption: nothing was read or written. Field
    /// `len` is the rejected length; field `max` is the limit.
    #[error("Key too large, len: {len} - max: {max}")]
    KeyTooLarge { len: usize, max: usize },

    /// `put` was given a value longer than `u32::MAX` bytes, the largest
    /// length a leaf cell can store. This is a caller error, not corruption:
    /// nothing was read or written. Field `len` is the rejected length; field
    /// `max` is the limit.
    #[error("Value too large, len: {len} - max: {max}")]
    ValueTooLarge { len: usize, max: usize },

    /// A structural check of [`verify_tree`](crate::storage::btree::verify::verify_tree)
    /// or [`verify_file`](crate::storage::btree::verify::verify_file) failed.
    /// Field `page` is the page where the problem was found; field `violation`
    /// names the rule that failed.
    #[error("Invariant violated, page_id: {page}, {violation}")]
    Invariant { page: PageId, violation: Violation },
}

/// The rule of ADR 0005 that a tree or a file breaks, reported by
/// [`BTreeError::Invariant`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Violation {
    /// A page is reachable twice: from two parents, from a cycle, or as a node
    /// and as an overflow page (invariant 7).
    #[error("page reached twice")]
    PageReachedTwice,

    /// A page reached from the tree is not a B+Tree node. Field `found` is its
    /// page type.
    #[error("page is not a B+Tree node, found page type: {found}")]
    NotANode { found: PageType },

    /// A descent is deeper than any valid tree.
    #[error("tree deeper than the maximum height")]
    TooDeep,

    /// A leaf is at a different depth than the first leaf found (invariant 2).
    /// Field `depth` is its depth; field `expected` is the depth of the others.
    #[error("leaf at depth {depth}, other leaves at {expected}")]
    LeafDepth { depth: usize, expected: usize },

    /// A key is outside the bounds that the separators above it give
    /// (invariant 4). Field `index` is the cell position.
    #[error("key {index} outside the bounds given by the parent")]
    KeyOutOfBounds { index: usize },

    /// A key is longer than `max_key_len`. Field `index` is the cell position.
    #[error("key {index} is {len} bytes, max {max}")]
    KeyTooLarge {
        index: usize,
        len: usize,
        max: usize,
    },

    /// The `next_leaf` of a leaf is not the next leaf of the walk (invariant 6).
    #[error("next_leaf is {found:?}, expected {expected:?}")]
    LeafLink {
        found: Option<PageId>,
        expected: Option<PageId>,
    },

    /// A non-root leaf has no cells (invariant 8).
    #[error("non-root leaf is empty")]
    EmptyLeaf,

    /// An internal node has no cells, so it has a single child (invariant 8).
    #[error("internal node has no cells")]
    EmptyInternal,

    /// A value is inline when it should be in a chain, or the other way round.
    /// Field `index` is the cell position.
    #[error("value of cell {index} is stored in the wrong form")]
    NonCanonicalValue { index: usize },

    /// An overflow page that is not the last of its chain holds less than a
    /// full page of data.
    #[error("overflow page is not full but not last")]
    OverflowNotFull,

    /// The chain of a cell holds a different number of bytes than the cell
    /// says. Field `index` is the cell position.
    #[error("overflow chain of cell {index} holds {found} bytes, the cell says {expected}")]
    OverflowLength {
        index: usize,
        expected: u32,
        found: usize,
    },

    /// Some pages of the file are in no tree and not on the free list (a leak),
    /// or in both. `reached` counts the pages of the trees, `free` the free
    /// list and `total` the pages of the file without the file header.
    #[error("{reached} pages reached + {free} free, file has {total}")]
    PageAccounting {
        reached: usize,
        free: u32,
        total: u32,
    },
}
