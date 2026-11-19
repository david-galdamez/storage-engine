//! Read and write views of a B+Tree internal node (ADR 0005).
//!
//! An internal node is a slotted page of type 5 with N separator keys and
//! N + 1 children. Each of the N cells holds a child and the key that bounds
//! it, and the last child lives in the header:
//!
//! | Offset | Size | Field                            |
//! |--------|------|----------------------------------|
//! | 24     | 4    | `rightmost_child: u32`, never 0  |
//! | 28     | 4    | reserved, 0                      |
//!
//! | Cell offset | Size | Field                    |
//! |-------------|------|--------------------------|
//! | 0           | 4    | `child: u32`, never 0    |
//! | 4           | rest | separator key            |
//!
//! Child positions run from 0 to N. Position `i < N` is the child of cell `i`,
//! and position N is the rightmost child. The subtree of child `i` holds the
//! keys `k` with `K(i-1) <= k < K(i)` (invariant 4), so a lookup of `k` goes to
//! the first position `i` with `k < K(i)`, or to N.
//!
//! A child page id of 0 is never valid. Reading one gives
//! [`NodeError::CorruptNode`], and so does passing one to a write operation.

use crate::storage::btree::{
    error::NodeError,
    node::{
        INTERNAL_CELL_HEADER_SIZE, binary_search, check_reserved_zero, internal_cell_child,
        internal_cell_key, max_key_len, page_size_of, read_link,
    },
};
use crate::storage::pager::{
    codec::{write_bytes, write_u32},
    error::PageError,
    page::{PageType, expect_page_type},
    slotted::{SlottedPage, SlottedPageMut},
    types::{PageId, PageSize},
};

/// Offset of `rightmost_child` within the header area (bytes 24..32).
const RIGHTMOST_OFFSET: usize = 0;

/// Rejects page 0, which is never a child.
const fn check_child(child: PageId) -> Result<(), NodeError> {
    if child.get() == 0 {
        return Err(NodeError::CorruptNode {
            reason: "child page id is 0",
        });
    }

    Ok(())
}

/// Read-only view of a B+Tree internal node, borrowing the page buffer.
#[derive(Debug, Clone, Copy)]
pub struct InternalNode<'a> {
    page: SlottedPage<'a>,
}

impl<'a> InternalNode<'a> {
    /// Creates a read view over `buffer`. Does not write to `buffer`.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::UnexpectedPageType`] (inside [`NodeError::Page`])
    /// with `expected` set to [`PageType::BTreeInternal`] if the page is not an
    /// internal node, and the errors of [`SlottedPage::new`] for the page size,
    /// an unknown type byte, or an inconsistent header.
    pub fn new(buffer: &'a [u8]) -> Result<Self, NodeError> {
        expect_page_type(buffer, PageType::BTreeInternal)?;

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

    /// Returns the number of keys N. The node has N + 1 children.
    #[must_use]
    pub fn cell_count(&self) -> usize {
        self.page.cell_count()
    }

    /// Returns the separator key at position `index`, borrowed from the page.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Page`] if `index` is out of range or the slot is
    /// invalid, and [`NodeError::CorruptNode`] if the cell is too short.
    pub fn key(&self, index: usize) -> Result<&'a [u8], NodeError> {
        internal_cell_key(self.page.cell(index)?)
    }

    /// Returns the rightmost child, the child at position N.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::CorruptNode`] if the stored page id is 0, and
    /// [`NodeError::Page`] if the header area cannot be read.
    pub fn rightmost_child(&self) -> Result<PageId, NodeError> {
        read_link(self.page.header_area()?, RIGHTMOST_OFFSET)?.ok_or(NodeError::CorruptNode {
            reason: "rightmost child is page 0",
        })
    }

    /// Returns the child at position `pos`, for `pos` in `0..=N`. Position N
    /// is the rightmost child.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::SlotOutOfRange`] (inside [`NodeError::Page`]) if
    /// `pos` is past N, and [`NodeError::CorruptNode`] if the stored page id
    /// is 0.
    pub fn child_at(&self, pos: usize) -> Result<PageId, NodeError> {
        let count = self.cell_count();
        if pos == count {
            return self.rightmost_child();
        }

        internal_cell_child(self.page.cell(pos)?)
    }

    /// Binary searches the separator keys in `memcmp` order, like
    /// `slice::binary_search`. Returns `Ok(index)` if `key` is a separator and
    /// `Err(index)` for the position where it would be inserted.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::key`].
    pub fn search(&self, key: &[u8]) -> Result<Result<usize, usize>, NodeError> {
        binary_search(self.cell_count(), |index| Ok(self.key(index)?.cmp(key)))
    }

    /// Returns the position and page id of the child whose subtree can hold
    /// `key` (invariant 4): the first position `i` with `key < K(i)`, or N if
    /// there is none. A key equal to a separator goes to the right. The
    /// position is what a breadcrumb stores.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::search`] and [`Self::child_at`].
    pub fn child_for(&self, key: &[u8]) -> Result<(usize, PageId), NodeError> {
        let pos = match self.search(key)? {
            Ok(index) => index.saturating_add(1),
            Err(index) => index,
        };

        Ok((pos, self.child_at(pos)?))
    }

    /// Checks the node: the slotted page invariants (ADR 0003), every child
    /// and the rightmost child are not 0, keys are in strictly ascending order
    /// (ADR 0005, invariant 3), and the reserved bytes 28..32 are zero.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Page`] if the slotted page is corrupt and
    /// [`NodeError::CorruptNode`] if a child, the key order or the reserved
    /// bytes are wrong.
    pub fn validate(&self) -> Result<(), NodeError> {
        self.page.validate()?;
        check_reserved_zero(self.page.header_area()?)?;
        self.rightmost_child()?;

        let mut previous: Option<&[u8]> = None;
        for index in 0..self.cell_count() {
            let cell = self.page.cell(index)?;
            internal_cell_child(cell)?;
            let key = internal_cell_key(cell)?;
            if previous.is_some_and(|prev| prev >= key) {
                return Err(NodeError::CorruptNode {
                    reason: "internal keys are not in strictly ascending order",
                });
            }
            previous = Some(key);
        }

        Ok(())
    }
}

/// Mutable view of a B+Tree internal node, borrowing the page buffer.
#[derive(Debug)]
pub struct InternalNodeMut<'a> {
    page: SlottedPageMut<'a>,
    page_size: PageSize,
}

impl<'a> InternalNodeMut<'a> {
    /// Formats `buffer` as an internal node with no keys and `rightmost` as its
    /// only child. Does not touch the checksum at offset 0.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::CorruptNode`] if `rightmost` is page 0, and the
    /// errors of [`SlottedPageMut::init_as`]. `buffer` is not modified in
    /// either case.
    pub fn init(buffer: &mut [u8], rightmost: PageId) -> Result<(), NodeError> {
        check_child(rightmost)?;
        SlottedPageMut::init_as(buffer, PageType::BTreeInternal)?;

        let mut page = SlottedPageMut::new(buffer)?;
        write_u32(page.header_area_mut()?, RIGHTMOST_OFFSET, rightmost.get())?;

        Ok(())
    }

    /// Creates a mutable view over `buffer`. Does not write to `buffer`.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`InternalNode::new`].
    pub fn new(buffer: &'a mut [u8]) -> Result<Self, NodeError> {
        expect_page_type(buffer, PageType::BTreeInternal)?;
        let page_size = page_size_of(buffer)?;

        Ok(Self {
            page: SlottedPageMut::new(buffer)?,
            page_size,
        })
    }

    /// Returns a read view of the same page buffer.
    #[must_use]
    pub const fn view(&self) -> InternalNode<'_> {
        InternalNode {
            page: self.page.view(),
        }
    }

    /// Inserts the separator `key` with its `child` at position `index`,
    /// encoding the cell in place. The node does not check the key order: the
    /// caller passes the index from [`InternalNode::search`]. A failed insert
    /// leaves the page unchanged, apart from a possible compaction that keeps
    /// the cells.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::KeyTooLarge`] if the key is longer than
    /// [`max_key_len`], [`NodeError::CorruptNode`] if `child` is page 0, and
    /// [`NodeError::Page`] for the errors of
    /// [`SlottedPageMut::insert_with`], such as a full page or an index past
    /// the cell count.
    pub fn insert(&mut self, index: usize, key: &[u8], child: PageId) -> Result<(), NodeError> {
        let max_key = max_key_len(self.page_size);
        if key.len() > max_key {
            return Err(NodeError::KeyTooLarge {
                len: key.len(),
                max: max_key,
            });
        }
        check_child(child)?;

        let len = INTERNAL_CELL_HEADER_SIZE.saturating_add(key.len());
        self.page.insert_with(index, len, |dst| {
            write_u32(dst, 0, child.get())?;
            write_bytes(dst, INTERNAL_CELL_HEADER_SIZE, key)
        })?;

        Ok(())
    }

    /// Removes the cell at position `index`: its key and its child.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Page`] with the errors of
    /// [`SlottedPageMut::remove`].
    pub fn remove(&mut self, index: usize) -> Result<(), NodeError> {
        self.page.remove(index)?;

        Ok(())
    }

    /// Sets the child at position `pos`, for `pos` in `0..=N`. Position N
    /// writes the rightmost pointer. The other positions change the first four
    /// bytes of the cell in place, so the key stays.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::CorruptNode`] if `child` is page 0,
    /// [`PageError::SlotOutOfRange`] (inside [`NodeError::Page`]) if `pos` is
    /// past N, and [`NodeError::Page`] if the cell cannot be written.
    pub fn set_child_at(&mut self, pos: usize, child: PageId) -> Result<(), NodeError> {
        check_child(child)?;

        let count = self.page.view().cell_count();
        if pos == count {
            return self.set_rightmost_child(child);
        }
        if pos > count {
            return Err(NodeError::Page(PageError::SlotOutOfRange {
                index: pos,
                cell_count: count,
            }));
        }

        write_u32(self.page.cell_mut(pos)?, 0, child.get())?;

        Ok(())
    }

    /// Sets the rightmost child. Writes bytes 24..28 only.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::CorruptNode`] if `child` is page 0, and
    /// [`NodeError::Page`] if the header area cannot be written.
    pub fn set_rightmost_child(&mut self, child: PageId) -> Result<(), NodeError> {
        check_child(child)?;
        write_u32(self.page.header_area_mut()?, RIGHTMOST_OFFSET, child.get())?;

        Ok(())
    }
}
