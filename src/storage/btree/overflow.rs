//! Overflow pages and chains (ADR 0005, options 5 and 7).
//!
//! A value that does not fit inline in a leaf cell lives in a chain of
//! overflow pages. The leaf cell holds the value length and the first page.
//! An overflow page (page type 6) is:
//!
//! | Offset          | Size       | Field                                  |
//! |-----------------|------------|----------------------------------------|
//! | 0..16           | 16         | common page header, type 6             |
//! | 16              | 4          | `next_overflow: u32`, 0 = end of chain |
//! | 20              | 4          | `data_len: u32`                        |
//! | 24              | `data_len` | value bytes                            |
//! | 24 + `data_len` | rest       | zero                                   |
//!
//! Every page of a chain except the last is full (`data_len` equals
//! [`overflow_capacity`]), and the sum of `data_len` over the chain is the
//! value length of the leaf cell.
//!
//! The module has two levels:
//!
//! - In-memory views with no I/O: [`OverflowPage`] (read) and
//!   [`OverflowPageMut`] (write). Errors are [`NodeError`].
//! - Chain operations over the pager: [`write_chain`], [`read_value`] and
//!   [`free_chain`]. Errors are [`BTreeError`]. Reading and freeing share one
//!   validating walker, so both reject the same corrupt chains.

use std::ops::Range;

use crate::storage::btree::{
    error::{BTreeError, NodeError},
    node::{ValueRef, page_buffer, page_size_of},
};
use crate::storage::pager::{
    codec::{read_u32, slice, write_bytes, write_u32},
    disk::Pager,
    page::{PageType, expect_page_type, page_type, set_page_type},
    types::{PageId, PageSize},
};

/// Size in bytes of the overflow page header: the common header, `next_overflow`
/// and `data_len`. The value bytes start here.
pub const OVERFLOW_HEADER_SIZE: usize = 24;

/// Offset of `next_overflow`.
const NEXT_OFFSET: usize = 16;

/// Offset of `data_len`.
const DATA_LEN_OFFSET: usize = 20;

/// Returns how many value bytes one overflow page of `page_size` holds:
/// `page_size - 24`.
#[must_use]
pub const fn overflow_capacity(page_size: PageSize) -> usize {
    page_size.as_usize().saturating_sub(OVERFLOW_HEADER_SIZE)
}

/// Read-only view of an overflow page, borrowing the page buffer.
#[derive(Debug, Clone, Copy)]
pub struct OverflowPage<'a> {
    buffer: &'a [u8],
    capacity: usize,
}

impl<'a> OverflowPage<'a> {
    /// Creates a read view over `buffer`. Does not write to `buffer`. It only
    /// checks the page type and the page size: call [`Self::validate`] for
    /// the rest.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::UnexpectedPageType`](crate::storage::pager::error::PageError::UnexpectedPageType)
    /// (inside [`NodeError::Page`]) with `expected` set to
    /// [`PageType::Overflow`] if the page has another type, and a page error
    /// if the buffer is not a valid page size or the type byte is unknown.
    pub fn new(buffer: &'a [u8]) -> Result<Self, NodeError> {
        expect_page_type(buffer, PageType::Overflow)?;
        let capacity = overflow_capacity(page_size_of(buffer)?);

        Ok(Self { buffer, capacity })
    }

    /// Returns the next page of the chain, or `None` for the last page.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::Page`] if the field cannot be read.
    pub fn next(&self) -> Result<Option<PageId>, NodeError> {
        Ok(PageId::from_disk(read_u32(self.buffer, NEXT_OFFSET)?))
    }

    /// Returns the value bytes held by this page, borrowed from the buffer.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::CorruptNode`] if `data_len` is larger than the
    /// capacity of the page, and [`NodeError::Page`] if the field cannot be
    /// read.
    pub fn data(&self) -> Result<&'a [u8], NodeError> {
        let data_len = usize::try_from(read_u32(self.buffer, DATA_LEN_OFFSET)?).map_err(|_| {
            NodeError::CorruptNode {
                reason: "overflow data_len does not fit in memory",
            }
        })?;
        if data_len > self.capacity {
            return Err(NodeError::CorruptNode {
                reason: "overflow data_len is larger than the page capacity",
            });
        }

        Ok(slice(self.buffer, OVERFLOW_HEADER_SIZE, data_len)?)
    }

    /// Checks the page: `data_len` is within the capacity and every byte after
    /// the data is zero.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::CorruptNode`] if `data_len` is too large or the
    /// tail of the page is not zero, and [`NodeError::Page`] if a field
    /// cannot be read.
    pub fn validate(&self) -> Result<(), NodeError> {
        let data_len = self.data()?.len();
        let tail = self
            .buffer
            .get(OVERFLOW_HEADER_SIZE.saturating_add(data_len)..)
            .unwrap_or_default();
        if tail.iter().any(|&b| b != 0) {
            return Err(NodeError::CorruptNode {
                reason: "overflow page bytes after the data are not zero",
            });
        }

        Ok(())
    }
}

/// Writer of overflow pages. It has no state: [`Self::init`] formats a whole
/// page buffer in one call, because an overflow page is never edited in place.
#[derive(Debug, Clone, Copy)]
pub struct OverflowPageMut;

impl OverflowPageMut {
    /// Formats `buffer` as an overflow page that holds `data` and links to
    /// `next` (`None` ends the chain). Sets the type and the header fields,
    /// copies `data` and zeroes every other byte. Does not touch the checksum:
    /// the pager stamps it on write.
    ///
    /// # Errors
    ///
    /// Returns [`NodeError::CellTooLarge`] if `data` is longer than
    /// [`overflow_capacity`], and [`NodeError::Page`] if `buffer` is not a
    /// valid page size. `buffer` is not modified in either case.
    pub fn init(buffer: &mut [u8], next: Option<PageId>, data: &[u8]) -> Result<(), NodeError> {
        let capacity = overflow_capacity(page_size_of(buffer)?);
        let too_large = NodeError::CellTooLarge {
            len: data.len(),
            max: capacity,
        };
        if data.len() > capacity {
            return Err(too_large);
        }
        let data_len = u32::try_from(data.len()).map_err(|_| too_large)?;

        buffer.fill(0);
        set_page_type(buffer, PageType::Overflow)?;
        write_u32(buffer, NEXT_OFFSET, PageId::to_disk(next))?;
        write_u32(buffer, DATA_LEN_OFFSET, data_len)?;
        write_bytes(buffer, OVERFLOW_HEADER_SIZE, data)?;

        Ok(())
    }
}

/// Writes `value` as a new chain of overflow pages and returns the first page.
///
/// The value is split into chunks of [`overflow_capacity`] bytes. The first
/// page is allocated, then for each chunk the next page is allocated before
/// the current one is written, so the chain follows the allocation order. One
/// page buffer is reused for all pages. Does not sync.
///
/// `value` must not be empty: a value that needs overflow never is.
///
/// There is no WAL yet (Phase 3). If an allocation or a write fails midway,
/// the pages already written are not reachable from any leaf and stay in the
/// file, unused.
///
/// # Errors
///
/// - [`BTreeError::CorruptTree`] (with page 0) if `value` is empty. This is a
///   caller bug, and nothing is allocated.
/// - [`BTreeError::Pager`] if an allocation or a write fails.
/// - [`BTreeError::Node`] if a page cannot be formatted.
pub(crate) fn write_chain(pager: &mut Pager, value: &[u8]) -> Result<PageId, BTreeError> {
    let mut chunks = value.chunks(overflow_capacity(pager.page_size()));
    let Some(mut chunk) = chunks.next() else {
        return Err(BTreeError::CorruptTree {
            page: PageId::HEADER,
            reason: "an empty value has no overflow chain",
        });
    };

    let first = pager.allocate_page()?;
    let mut current = first;
    let mut buffer = page_buffer(pager);
    loop {
        let node_error = |source| BTreeError::Node {
            page: current,
            source,
        };
        if let Some(next_chunk) = chunks.next() {
            let next = pager.allocate_page()?;
            OverflowPageMut::init(&mut buffer, Some(next), chunk).map_err(node_error)?;
            pager.write_page(current, &mut buffer)?;
            current = next;
            chunk = next_chunk;
        } else {
            OverflowPageMut::init(&mut buffer, None, chunk).map_err(node_error)?;
            pager.write_page(current, &mut buffer)?;
            return Ok(first);
        }
    }
}

/// Appends the value described by `value` to `out`.
///
/// It copies the bytes of an inline value, or reads the whole overflow chain.
/// `out` is not cleared, so the caller can reuse one vector for many values
/// (ADR 0005, option 7).
///
/// For an overflow value it reserves the space once and reads one page at a
/// time into a single page buffer. The chain is validated while it is read
/// (see [`BTreeError::CorruptTree`] below). On error `out` is truncated back to
/// its length at the call.
///
/// # Errors
///
/// - [`BTreeError::Pager`] if a read fails, a checksum is wrong or a page is
///   past the end of the file.
/// - [`BTreeError::Node`] if an overflow page does not decode or validate.
/// - [`BTreeError::CorruptTree`] if a page of the chain is not an overflow
///   page, a page but the last is not full, or the chain is shorter or longer
///   than the length in the leaf cell (this also rules out cycles).
pub fn read_value(pager: &Pager, value: ValueRef<'_>, out: &mut Vec<u8>) -> Result<(), BTreeError> {
    match value {
        ValueRef::Inline(bytes) => {
            out.extend_from_slice(bytes);
            Ok(())
        }
        ValueRef::Overflow { len, first_page } => {
            let start = out.len();
            let result = read_chain(pager, len, first_page, out);
            if result.is_err() {
                out.truncate(start);
            }
            result
        }
    }
}

/// Reads the chain of `len` bytes that starts at `first` into `out`.
fn read_chain(pager: &Pager, len: u32, first: PageId, out: &mut Vec<u8>) -> Result<(), BTreeError> {
    let mut walker = ChainWalker::new(pager, len, first)?;
    // A valid chain cannot hold more than the file does, so a corrupt length
    // does not reserve more than that.
    let file_capacity = usize::try_from(pager.page_count())
        .unwrap_or(usize::MAX)
        .saturating_mul(overflow_capacity(pager.page_size()));
    out.reserve(walker.remaining.min(file_capacity));

    let mut buffer = page_buffer(pager);
    while let Some(step) = walker.step(pager, &mut buffer)? {
        let data = buffer
            .get(step.data.clone())
            .ok_or(BTreeError::CorruptTree {
                page: step.page,
                reason: "overflow data is outside its page",
            })?;
        out.extend_from_slice(data);
    }

    Ok(())
}

/// Frees every page of the chain of `len` bytes that starts at `first`.
///
/// It walks the chain once. For each page it reads and validates the page,
/// remembers its `next` link and only then frees it, because freeing
/// overwrites the page (ADR 0003). The chain is validated as it goes, not
/// before: if a page in the middle is corrupt, the pages before it are already
/// freed when the error is returned. A second validating pass would avoid
/// that at the cost of reading the chain twice. Corruption is rare and there
/// is no WAL yet (Phase 3), so the single pass is used. Does not sync.
///
/// # Errors
///
/// The errors of [`read_value`] for a bad chain, and [`BTreeError::Pager`] if a
/// page cannot be freed.
pub(crate) fn free_chain(pager: &mut Pager, len: u32, first: PageId) -> Result<(), BTreeError> {
    let mut walker = ChainWalker::new(pager, len, first)?;
    let mut buffer = page_buffer(pager);
    while let Some(step) = walker.step(pager, &mut buffer)? {
        pager.free_page(step.page)?;
    }

    Ok(())
}

/// One validated page of a chain: its id and where its data is in the buffer
/// that `ChainWalker::step` filled.
struct ChainStep {
    page: PageId,
    data: Range<usize>,
}

/// Walks a chain page by page and checks its structure against the length in
/// the leaf cell. It does not hold the pager, so the caller can free the page
/// of a step (with `&mut Pager`) before asking for the next one.
///
/// Termination: every page but the last has `data_len == capacity > 0`, and
/// every step subtracts `data_len` from `remaining`, which is checked. So the
/// walk makes at most `ceil(len / capacity)` steps even if the links form a
/// cycle: the cycle ends in an error as soon as `remaining` runs out.
struct ChainWalker {
    /// The page to read next, `None` when the chain is done.
    next: Option<PageId>,
    /// Value bytes not seen yet.
    remaining: usize,
    capacity: usize,
}

impl ChainWalker {
    /// Starts a walk over a chain of `len` bytes at `first`. Reads nothing.
    fn new(pager: &Pager, len: u32, first: PageId) -> Result<Self, BTreeError> {
        let remaining = usize::try_from(len).map_err(|_| BTreeError::CorruptTree {
            page: first,
            reason: "overflow value length does not fit in memory",
        })?;
        if remaining == 0 {
            return Err(BTreeError::CorruptTree {
                page: first,
                reason: "overflow value has length 0",
            });
        }

        Ok(Self {
            next: Some(first),
            remaining,
            capacity: overflow_capacity(pager.page_size()),
        })
    }

    /// Reads and validates the next page into `buffer`. Returns `None` when
    /// the chain is complete.
    fn step(&mut self, pager: &Pager, buffer: &mut [u8]) -> Result<Option<ChainStep>, BTreeError> {
        let Some(id) = self.next else {
            return Ok(None);
        };
        let corrupt = |reason| BTreeError::CorruptTree { page: id, reason };
        let node_error = |source| BTreeError::Node { page: id, source };

        pager.read_page(id, buffer)?;
        if !matches!(page_type(buffer), Ok(PageType::Overflow)) {
            return Err(corrupt("overflow chain page is not an overflow page"));
        }
        let page = OverflowPage::new(buffer).map_err(node_error)?;
        page.validate().map_err(node_error)?;
        let data_len = page.data().map_err(node_error)?.len();
        let next = page.next().map_err(node_error)?;

        if next.is_some() && data_len != self.capacity {
            return Err(corrupt("a non-last overflow page is not full"));
        }
        let remaining = self
            .remaining
            .checked_sub(data_len)
            .ok_or_else(|| corrupt("overflow chain is longer than the value length"))?;
        if next.is_none() && remaining != 0 {
            return Err(corrupt("overflow chain ends before the value length"));
        }
        if next.is_some() && remaining == 0 {
            return Err(corrupt("overflow chain continues past the value length"));
        }

        self.remaining = remaining;
        self.next = next;

        Ok(Some(ChainStep {
            page: id,
            data: OVERFLOW_HEADER_SIZE..OVERFLOW_HEADER_SIZE.saturating_add(data_len),
        }))
    }
}
