//! Read and write views of a B+Tree leaf node (ADR 0005).
//!
//! A leaf is a slotted page of type 4. Its cells are keys with values in
//! strictly ascending byte order (see [`crate::storage::btree::node`] for the
//! cell layout). Bytes 24..32 of the header are:
//!
//! | Offset | Size | Field                      |
//! |--------|------|----------------------------|
//! | 24     | 4    | `next_leaf: u32`, 0 = none |
//! | 28     | 4    | reserved, 0                |
//!
//! [`LeafNode`] reads a leaf through a shared `&[u8]`. [`LeafNodeMut`] reads
//! and writes through a `&mut [u8]`. Both borrow the page buffer, and cells and
//! values are returned as slices of it.

use crate::storage::btree::{
    error::NodeError,
    node::{
        LeafCell, ValueRef, binary_search, check_reserved_zero, decode_leaf_cell, encode_leaf_cell,
        leaf_cell_key, leaf_cell_len, max_key_len, max_node_cell_len, page_size_of, read_link,
    },
};
use crate::storage::pager::{
    codec::write_u32,
    page::{PageType, expect_page_type},
    slotted::{SlottedPage, SlottedPageMut},
    types::{PageId, PageSize},
};

/// Offset of `next_leaf` within the header area (bytes 24..32).
const NEXT_LEAF_OFFSET: usize = 0;

/// Read-only view of a B+Tree leaf, borrowing the page buffer.
#[derive(Debug, Clone, Copy)]
pub struct LeafNode<'a> {
    page: SlottedPage<'a>,
}

impl<'a> LeafNode<'a> {
    /// Creates a read view over `buffer`. Does not write to `buffer`.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::UnexpectedPageType`](crate::storage::pager::error::PageError::UnexpectedPageType)
    /// (inside [`NodeError::Page`]) with `expected` set to
    /// [`PageType::BTreeLeaf`] if the page is not a leaf, and the errors of
    /// [`SlottedPage::new`] for the page size, an unknown type byte, or an
    /// inconsistent header.
    pub fn new(buffer: &'a [u8]) -> Result<Self, NodeError> {
        expect_page_type(buffer, PageType::BTreeLeaf)?;

        Ok(Self {
            page: SlottedPage::new(buffer)?,
        })
    }

    /// Returns the slotted page view, for free space queries such as a split
    /// decision.
    #[must_use]
    pub const fn slotted(&self) -> SlottedPage<'a> {
        self.page
    }

    /// Returns the number of cells in the leaf.
    #[must_use]
    pub fn cell_count(&self) -> usize {
        self.page.cell_count()
    }

    /// Returns the decoded cell at position `index`, borrowed from the page.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Page`] if `index` is out of range or the slot is
    /// invalid, and [`NodeError::CorruptNode`] if the cell does not decode.
    pub fn cell(&self, index: usize) -> Result<LeafCell<'a>, NodeError> {
        decode_leaf_cell(self.page.cell(index)?)
    }

    /// Returns the key at position `index`, borrowed from the page.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Page`] if `index` is out of range or the slot is
    /// invalid, and [`NodeError::CorruptNode`] if the key does not decode.
    pub fn key(&self, index: usize) -> Result<&'a [u8], NodeError> {
        leaf_cell_key(self.page.cell(index)?)
    }

    /// Binary searches the keys in `memcmp` order, like `slice::binary_search`.
    /// Returns `Ok(index)` if `key` is in the leaf and `Err(index)` for the
    /// position where it would be inserted.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::key`].
    pub fn search(&self, key: &[u8]) -> Result<Result<usize, usize>, NodeError> {
        binary_search(self.cell_count(), |index| Ok(self.key(index)?.cmp(key)))
    }

    /// Returns the leaf to the right of this one, or `None` for the last leaf.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Page`] if the header area cannot be read.
    pub fn next_leaf(&self) -> Result<Option<PageId>, NodeError> {
        read_link(self.page.header_area()?, NEXT_LEAF_OFFSET)
    }

    /// Checks the leaf: the slotted page invariants (ADR 0003), every cell
    /// decodes, keys are in strictly ascending order (ADR 0005, invariant 3),
    /// and the reserved bytes 28..32 are zero.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Page`] if the slotted page is corrupt and
    /// [`NodeError::CorruptNode`] if a cell, the key order or the reserved
    /// bytes are wrong.
    pub fn validate(&self) -> Result<(), NodeError> {
        self.page.validate()?;
        check_reserved_zero(self.page.header_area()?)?;

        let mut previous: Option<&[u8]> = None;
        for index in 0..self.cell_count() {
            let cell = self.cell(index)?;
            if previous.is_some_and(|prev| prev >= cell.key) {
                return Err(NodeError::CorruptNode {
                    reason: "leaf keys are not in strictly ascending order",
                });
            }
            previous = Some(cell.key);
        }

        Ok(())
    }
}

/// Mutable view of a B+Tree leaf, borrowing the page buffer.
#[derive(Debug)]
pub struct LeafNodeMut<'a> {
    page: SlottedPageMut<'a>,
    page_size: PageSize,
}

impl<'a> LeafNodeMut<'a> {
    /// Formats `buffer` as an empty leaf with no next leaf. Does not touch the
    /// checksum at offset 0.
    ///
    /// # Errors
    ///
    /// Returns the errors of
    /// [`SlottedPageMut::init_as`](crate::storage::pager::slotted::SlottedPageMut::init_as).
    /// `buffer` is not modified in that case.
    pub fn init(buffer: &mut [u8]) -> Result<(), NodeError> {
        SlottedPageMut::init_as(buffer, PageType::BTreeLeaf)?;

        Ok(())
    }

    /// Creates a mutable view over `buffer`. Does not write to `buffer`.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`LeafNode::new`].
    pub fn new(buffer: &'a mut [u8]) -> Result<Self, NodeError> {
        expect_page_type(buffer, PageType::BTreeLeaf)?;
        let page_size = page_size_of(buffer)?;

        Ok(Self {
            page: SlottedPageMut::new(buffer)?,
            page_size,
        })
    }

    /// Returns a read view of the same page buffer.
    #[must_use]
    pub const fn view(&self) -> LeafNode<'_> {
        LeafNode {
            page: self.page.view(),
        }
    }

    /// Inserts `key` with `value` at position `index`, encoding the cell in
    /// place. The node does not check the key order: the caller passes the
    /// index from [`LeafNode::search`]. A failed insert leaves the page
    /// unchanged, apart from a possible compaction that keeps the cells.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::KeyTooLarge`] if the key is longer than
    /// [`max_key_len`], [`NodeError::CellTooLarge`] if the cell is longer than
    /// [`max_node_cell_len`] (the value should have gone to overflow),
    /// [`NodeError::CorruptNode`] for an overflow value whose first page is 0,
    /// and [`NodeError::Page`] for the errors of
    /// [`SlottedPageMut::insert_with`], such as a full page or an index past
    /// the cell count.
    pub fn insert(
        &mut self,
        index: usize,
        key: &[u8],
        value: ValueRef<'_>,
    ) -> Result<(), NodeError> {
        let max_key = max_key_len(self.page_size);
        if key.len() > max_key {
            return Err(NodeError::KeyTooLarge {
                len: key.len(),
                max: max_key,
            });
        }

        if let ValueRef::Overflow { first_page, .. } = value
            && first_page.get() == 0
        {
            return Err(NodeError::CorruptNode {
                reason: "overflow value points to page 0",
            });
        }

        let len = leaf_cell_len(key.len(), &value);
        let max_cell = max_node_cell_len(self.page_size);
        if len > max_cell {
            return Err(NodeError::CellTooLarge { len, max: max_cell });
        }

        self.page
            .insert_with(index, len, |dst| encode_leaf_cell(dst, key, &value))?;

        Ok(())
    }

    /// Removes the cell at position `index`.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Page`] with the errors of
    /// [`SlottedPageMut::remove`].
    pub fn remove(&mut self, index: usize) -> Result<(), NodeError> {
        self.page.remove(index)?;

        Ok(())
    }

    /// Sets the leaf to the right of this one. `None` marks the last leaf.
    /// Writes bytes 24..28 only.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Page`] if the header area cannot be written.
    pub fn set_next_leaf(&mut self, next: Option<PageId>) -> Result<(), NodeError> {
        write_u32(
            self.page.header_area_mut()?,
            NEXT_LEAF_OFFSET,
            PageId::to_disk(next),
        )?;

        Ok(())
    }
}
