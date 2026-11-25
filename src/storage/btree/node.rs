//! Size limits and the leaf cell codec shared by the node views.
//!
//! A B+Tree node is a slotted page (ADR 0003) whose type byte is 4 (leaf) or 5
//! (internal) and whose bytes 24..32 are node specific (ADR 0005):
//!
//! | Offset | Size | Leaf                       | Internal                        |
//! |--------|------|----------------------------|---------------------------------|
//! | 24     | 4    | `next_leaf: u32`, 0 = none | `rightmost_child: u32`, never 0 |
//! | 28     | 4    | reserved, 0                | reserved, 0                     |
//!
//! A leaf cell is:
//!
//! | Offset      | Size      | Field                                       |
//! |-------------|-----------|---------------------------------------------|
//! | 0           | 2         | `key_len: u16`                              |
//! | 2           | 1         | `flags: u8`: bit 0 = overflow, others 0     |
//! | 3           | `key_len` | key                                         |
//! | 3 + `key_len` | rest    | inline: the value (may be empty)            |
//! | 3 + `key_len` | 4       | overflow: `value_len: u32`                  |
//! | 7 + `key_len` | 4       | overflow: `first_overflow: u32`, never 0    |
//!
//! An overflow cell is exactly `11 + key_len` bytes. All integers are
//! little-endian.
//!
//! The limits follow ADR 0005, option 6: an entry (cell plus its slot) uses at
//! most a quarter of the usable space of the page, so a split always gives two
//! nodes that fit.

use std::cmp::Ordering;

use crate::storage::btree::error::NodeError;
use crate::storage::pager::{
    codec::{read_u8, read_u16, read_u32, slice, write_bytes, write_u8, write_u16, write_u32},
    error::PageError,
    slotted::{SLOT_SIZE, SLOTTED_HEADER_SIZE},
    types::{PageId, PageSize},
};

/// Size in bytes of the fixed part of a leaf cell: `key_len` and `flags`.
pub const LEAF_CELL_HEADER_SIZE: usize = 3;

/// Size in bytes of the overflow reference that follows the key in an
/// overflow leaf cell: `value_len` and the first overflow page.
pub const OVERFLOW_REF_SIZE: usize = 8;

/// Size in bytes of the child pointer at the start of an internal cell.
pub const INTERNAL_CELL_HEADER_SIZE: usize = 4;

/// Leaf cell flag: the value is out of line.
const FLAG_OVERFLOW: u8 = 1;

/// Returns the largest cell a node of `page_size` accepts.
///
/// It is a quarter of the usable space, minus the slot (ADR 0005, option 6):
/// 116, 1012 and 16372 for 512, 4096 and 65536.
#[must_use]
pub const fn max_node_cell_len(page_size: PageSize) -> usize {
    let usable = page_size.as_usize().saturating_sub(SLOTTED_HEADER_SIZE);
    (usable / 4).saturating_sub(SLOT_SIZE)
}

/// Returns the usable bytes of a node of `page_size`: the page minus the
/// common header (32 bytes). Cells and slots live there.
#[must_use]
pub const fn usable_space(page_size: PageSize) -> usize {
    page_size.as_usize().saturating_sub(SLOTTED_HEADER_SIZE)
}

/// Returns the smallest number of used bytes (cells plus slots) that a
/// non-root node of `page_size` may have without being underfull.
///
/// It is a quarter of the usable space, rounded up (ADR 0005, amendment 3): a
/// node is underfull when `used * 4 < usable`, that is, when `used` is below
/// this value. It is 120, 1016 and 16376 for 512, 4096 and 65536.
#[must_use]
pub const fn min_node_fill(page_size: PageSize) -> usize {
    usable_space(page_size).div_ceil(4)
}

/// Returns the largest key a node of `page_size` accepts.
///
/// It is the largest cell minus the 11 bytes of an overflow leaf cell, so every
/// key fits in a leaf and in an internal cell. It is 105, 1001 and 16361 for
/// 512, 4096 and 65536.
#[must_use]
pub const fn max_key_len(page_size: PageSize) -> usize {
    max_node_cell_len(page_size)
        .saturating_sub(LEAF_CELL_HEADER_SIZE.saturating_add(OVERFLOW_REF_SIZE))
}

/// Returns whether a value of `value_len` bytes is stored inline next to a key
/// of `key_len` bytes.
///
/// The rule is `3 + key_len + value_len <= max_node_cell_len`. Otherwise the value goes to an overflow chain. Huge inputs do not overflow.
#[must_use]
pub const fn fits_inline(page_size: PageSize, key_len: usize, value_len: usize) -> bool {
    match key_len.checked_add(value_len) {
        Some(sum) => match sum.checked_add(LEAF_CELL_HEADER_SIZE) {
            Some(cell_len) => cell_len <= max_node_cell_len(page_size),
            None => false,
        },
        None => false,
    }
}

/// How a leaf cell holds its value: a slice of the page, or a reference to an
/// overflow chain (ADR 0005, option 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueRef<'a> {
    /// The value bytes, borrowed from the page. May be empty.
    Inline(&'a [u8]),
    /// The value is in a chain of overflow pages.
    Overflow {
        /// Length of the whole value in bytes.
        len: u32,
        /// First page of the chain, never page 0.
        first_page: PageId,
    },
}

/// A decoded leaf cell, borrowed from the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeafCell<'a> {
    /// The key bytes.
    pub key: &'a [u8],
    /// The value, inline or a reference to an overflow chain.
    pub value: ValueRef<'a>,
}

/// Returns the length in bytes of the leaf cell for `key_len` and `value`.
/// Saturates for lengths that cannot be a real cell.
#[must_use]
pub const fn leaf_cell_len(key_len: usize, value: &ValueRef<'_>) -> usize {
    let key_part = key_len.saturating_add(LEAF_CELL_HEADER_SIZE);
    match value {
        ValueRef::Inline(bytes) => key_part.saturating_add(bytes.len()),
        ValueRef::Overflow { .. } => key_part.saturating_add(OVERFLOW_REF_SIZE),
    }
}

/// Reads the key of a leaf cell without decoding the value.
///
/// # Errors
///
/// Returns [`NodeError::CorruptNode`] if the cell is shorter than its header
/// or the key runs past the end of the cell.
pub fn leaf_cell_key(cell: &[u8]) -> Result<&[u8], NodeError> {
    if cell.len() < LEAF_CELL_HEADER_SIZE {
        return Err(NodeError::CorruptNode {
            reason: "leaf cell shorter than its header",
        });
    }
    let key_len = usize::from(read_u16(cell, 0)?);
    cell.get(LEAF_CELL_HEADER_SIZE..LEAF_CELL_HEADER_SIZE.saturating_add(key_len))
        .ok_or(NodeError::CorruptNode {
            reason: "leaf cell key past the end of the cell",
        })
}

/// Decodes a leaf cell. The result borrows from `cell`.
///
/// # Errors
///
/// Returns [`NodeError::CorruptNode`] if the cell is shorter than its header,
/// the key runs past the end of the cell, `flags` has a bit other than bit 0,
/// an overflow cell is not exactly `11 + key_len` bytes, or its first overflow
/// page is 0.
pub fn decode_leaf_cell(cell: &[u8]) -> Result<LeafCell<'_>, NodeError> {
    let key = leaf_cell_key(cell)?;
    let flags = read_u8(cell, 2)?;
    if flags & !FLAG_OVERFLOW != 0 {
        return Err(NodeError::CorruptNode {
            reason: "leaf cell has unknown flags",
        });
    }

    let key_end = LEAF_CELL_HEADER_SIZE.saturating_add(key.len());
    if flags & FLAG_OVERFLOW == 0 {
        let value = cell.get(key_end..).ok_or(NodeError::CorruptNode {
            reason: "leaf cell value past the end of the cell",
        })?;
        return Ok(LeafCell {
            key,
            value: ValueRef::Inline(value),
        });
    }

    if cell.len() != key_end.saturating_add(OVERFLOW_REF_SIZE) {
        return Err(NodeError::CorruptNode {
            reason: "overflow leaf cell has the wrong length",
        });
    }
    let len = read_u32(cell, key_end)?;
    let first_page = PageId::from_disk(read_u32(cell, key_end.saturating_add(4))?).ok_or(
        NodeError::CorruptNode {
            reason: "overflow leaf cell points to page 0",
        },
    )?;

    Ok(LeafCell {
        key,
        value: ValueRef::Overflow { len, first_page },
    })
}

/// Encodes a leaf cell into `dst`, which must be exactly
/// [`leaf_cell_len`] bytes. The caller checks the limits.
///
/// # Errors
///
/// Returns [`PageError::OutOfBounds`] if `dst` is too short and
/// [`PageError::CellTooLarge`] if the key length does not fit in a `u16`.
pub(crate) fn encode_leaf_cell(
    dst: &mut [u8],
    key: &[u8],
    value: &ValueRef<'_>,
) -> Result<(), PageError> {
    let key_len = u16::try_from(key.len()).map_err(|_| PageError::CellTooLarge {
        len: key.len(),
        max: usize::from(u16::MAX),
    })?;
    let key_end = LEAF_CELL_HEADER_SIZE.saturating_add(key.len());

    write_u16(dst, 0, key_len)?;
    write_bytes(dst, LEAF_CELL_HEADER_SIZE, key)?;
    match value {
        ValueRef::Inline(bytes) => {
            write_u8(dst, 2, 0)?;
            write_bytes(dst, key_end, bytes)
        }
        ValueRef::Overflow { len, first_page } => {
            write_u8(dst, 2, FLAG_OVERFLOW)?;
            write_u32(dst, key_end, *len)?;
            write_u32(dst, key_end.saturating_add(4), first_page.get())
        }
    }
}

/// Reads the child pointer of an internal cell.
pub(crate) fn internal_cell_child(cell: &[u8]) -> Result<PageId, NodeError> {
    if cell.len() < INTERNAL_CELL_HEADER_SIZE {
        return Err(NodeError::CorruptNode {
            reason: "internal cell shorter than its child pointer",
        });
    }
    PageId::from_disk(read_u32(cell, 0)?).ok_or(NodeError::CorruptNode {
        reason: "child page id is 0",
    })
}

/// Reads the separator key of an internal cell.
pub(crate) fn internal_cell_key(cell: &[u8]) -> Result<&[u8], NodeError> {
    cell.get(INTERNAL_CELL_HEADER_SIZE..)
        .ok_or(NodeError::CorruptNode {
            reason: "internal cell shorter than its child pointer",
        })
}

/// Returns the page size of `buffer`, based on its length.
pub(crate) fn page_size_of(buffer: &[u8]) -> Result<PageSize, NodeError> {
    Ok(PageSize::new(
        u32::try_from(buffer.len()).unwrap_or(u32::MAX),
    )?)
}

/// Reads a page id stored in the header area at `offset`, 0 meaning none.
pub(crate) fn read_link(area: &[u8], offset: usize) -> Result<Option<PageId>, NodeError> {
    Ok(PageId::from_disk(read_u32(area, offset)?))
}

/// Checks that the reserved bytes 28..32 of the header area are zero.
pub(crate) fn check_reserved_zero(area: &[u8]) -> Result<(), NodeError> {
    if slice(area, 4, 4)?.iter().any(|&b| b != 0) {
        return Err(NodeError::CorruptNode {
            reason: "reserved bytes 28..32 are not zero",
        });
    }
    Ok(())
}

/// Binary search over `count` entries in ascending order, like
/// `slice::binary_search_by`: `compare(i)` says how entry `i` compares to the
/// target. Returns `Ok(index)` for a match and `Err(index)` for the position
/// where the target would be inserted.
pub(crate) fn binary_search<F>(
    count: usize,
    mut compare: F,
) -> Result<Result<usize, usize>, NodeError>
where
    F: FnMut(usize) -> Result<Ordering, NodeError>,
{
    let mut lo = 0;
    let mut hi = count;

    while lo < hi {
        let mid = lo.saturating_add(hi.saturating_sub(lo).saturating_div(2));
        match compare(mid)? {
            Ordering::Less => lo = mid.saturating_add(1),
            Ordering::Greater => hi = mid,
            Ordering::Equal => return Ok(Ok(mid)),
        }
    }

    Ok(Err(lo))
}
