//! Slotted page layout for variable-size cells.
//!
//! A slotted page stores variable-size cells (byte strings) in one page. The
//! layout is described in `docs/decisions/0003-pager-file-format.md`:
//!
//! - A 32-byte header: the common page header, then the cell count, the
//!   fragmented byte count, and `free_end`.
//! - A slot directory right after the header. Each 4-byte slot holds the
//!   offset and length of one cell. Slots are in logical order, so a binary
//!   search works directly on them.
//! - The cell area at the end of the page. Cells grow backward from the end of
//!   the page, and the free space sits between the slot directory and the cells.
//!   Removed cells leave holes, counted in `fragmented_bytes`, until
//!   [`SlottedPageMut::compact`] rewrites the cells without them.
//!
//! The layout is shared by every page type with a slotted layout
//! ([`PageType::has_slotted_layout`]): plain slotted pages and the B+Tree nodes
//! of ADR 0005. The views accept any of them. A caller that needs one type
//! (a node view) checks it itself. Bytes 24..32 belong to the caller: no
//! operation here writes them.
//!
//! The module exposes two borrowed views over the page buffer:
//!
//! - [`SlottedPage`] reads a page through a shared `&[u8]`.
//! - [`SlottedPageMut`] reads and writes a page through a `&mut [u8]`. Its
//!   [`SlottedPageMut::view`] method returns a read view of the same buffer.

use std::cmp::Ordering;

use crate::storage::pager::{
    codec::{read_u16, read_u32, slice, write_bytes, write_u16, write_u32},
    error::PageError,
    page::{PageType, page_type, set_page_type},
    types::PageSize,
};

/// Size in bytes of the slotted page header, including the common page header.
/// The slot directory starts at this offset.
pub const SLOTTED_HEADER_SIZE: usize = 32;

/// Size in bytes of one slot directory entry: a `u16` cell offset and a `u16`
/// cell length.
pub const SLOT_SIZE: usize = 4;

/// Offset of the caller-owned area of the header (bytes 24..32). No operation
/// of this module writes it; B+Tree nodes keep their sibling link or rightmost
/// child there (ADR 0005).
pub const HEADER_AREA_OFFSET: usize = 24;

/// Size in bytes of the caller-owned area of the header.
pub const HEADER_AREA_SIZE: usize = 8;

/// Offset of the cell length within a slot entry.
const SLOT_LEN_OFFSET: usize = 2;

/// Offset of the `cell_count` field (`u16`) in the page header.
const CELL_COUNT_OFFSET: usize = 16;

/// Offset of the `fragmented_bytes` field (`u16`) in the page header.
const FRAGMENTED_BYTES_OFFSET: usize = 18;

/// Offset of the `free_end` field (`u32`) in the page header.
const FREE_END_OFFSET: usize = 20;

/// Returns the largest cell that fits on an empty page of `size`, which is the
/// page size minus the slotted header and one slot entry.
#[must_use]
pub const fn max_cell_len(size: PageSize) -> usize {
    (size.as_usize().saturating_sub(SLOTTED_HEADER_SIZE)).saturating_sub(SLOT_SIZE)
}

/// Returns the page size of `buffer`, based on its length.
fn page_size_for(buffer: &[u8]) -> Result<PageSize, PageError> {
    PageSize::new(u32::try_from(buffer.len()).unwrap_or(u32::MAX))
}

/// Checks the page size, the page type, and the header fields of a slotted
/// page. Does not write to `buffer`.
fn check_header(buffer: &[u8]) -> Result<(), PageError> {
    let page_size = page_size_for(buffer)?;
    let found = page_type(buffer)?;
    if !found.has_slotted_layout() {
        return Err(PageError::UnexpectedPageType {
            expected: PageType::Slotted,
            found,
        });
    }

    let free_end = read_u32(buffer, FREE_END_OFFSET)?;
    if free_end > page_size.get() {
        return Err(PageError::CorruptSlottedPage {
            reason: "free_end past page_size",
        });
    }

    let fragmented_bytes = read_u16(buffer, FRAGMENTED_BYTES_OFFSET)?;
    if u32::from(fragmented_bytes) > page_size.get().saturating_sub(free_end) {
        return Err(PageError::CorruptSlottedPage {
            reason: "fragmented_bytes past page_size",
        });
    }

    let count = read_u16(buffer, CELL_COUNT_OFFSET)?;
    let free_start =
        (SLOT_SIZE.saturating_mul(usize::from(count))).saturating_add(SLOTTED_HEADER_SIZE);

    if free_start > usize::try_from(free_end).unwrap_or(usize::MAX) {
        return Err(PageError::CorruptSlottedPage {
            reason: "free_start past free_end",
        });
    }

    Ok(())
}

/// Read-only view of a slotted page, borrowing the page buffer.
#[derive(Debug, Clone, Copy)]
pub struct SlottedPage<'a> {
    page: &'a [u8],
}

impl<'a> SlottedPage<'a> {
    /// Creates a read view over `buffer`. Does not write to `buffer`. Accepts
    /// any page type with a slotted layout (see [`PageType::has_slotted_layout`]).
    ///
    /// # Errors
    ///
    /// Returns [`PageError::InvalidPageSize`] if `buffer` is not a valid page
    /// size, [`PageError::UnknownPageType`] if the type byte is unknown,
    /// [`PageError::UnexpectedPageType`] (with `expected` set to
    /// [`PageType::Slotted`]) if the page type has no slotted layout, and [`PageError::CorruptSlottedPage`]
    /// if the header fields are inconsistent.
    pub fn new(buffer: &'a [u8]) -> Result<Self, PageError> {
        check_header(buffer)?;

        Ok(Self { page: buffer })
    }

    /// Returns the bytes of the cell at logical position `index`, borrowed from
    /// the page.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::SlotOutOfRange`] if `index` is not below the cell
    /// count, and [`PageError::CorruptSlottedPage`] if the slot at `index` is
    /// invalid.
    pub fn cell(&self, index: usize) -> Result<&'a [u8], PageError> {
        if index >= self.cell_count() {
            return Err(PageError::SlotOutOfRange {
                index,
                cell_count: self.cell_count(),
            });
        }

        let (cell_offset, end) = self.slot(index)?;

        self.page
            .get(cell_offset..end)
            .ok_or(PageError::CorruptSlottedPage {
                reason: "cell not in range of page",
            })
    }

    /// Reads the slot at `index` and returns its cell range as `(offset, end)`.
    /// Checks that the slot is not empty and that the cell lies in the cell area.
    fn slot(&self, index: usize) -> Result<(usize, usize), PageError> {
        let slot_offset = (index.saturating_mul(SLOT_SIZE)).saturating_add(SLOTTED_HEADER_SIZE);
        let cell_offset = usize::from(read_u16(self.page, slot_offset)?);
        let cell_len = usize::from(read_u16(
            self.page,
            slot_offset.saturating_add(SLOT_LEN_OFFSET),
        )?);
        if cell_len == 0 {
            return Err(PageError::CorruptSlottedPage {
                reason: "cell_len is 0",
            });
        }

        if cell_offset < self.free_end() {
            return Err(PageError::CorruptSlottedPage {
                reason: "cell_offset before free_end",
            });
        }

        let end = cell_offset
            .checked_add(cell_len)
            .ok_or(PageError::CorruptSlottedPage {
                reason: "cell_offset plus cell_len overflows",
            })?;
        if end > self.page.len() {
            return Err(PageError::CorruptSlottedPage {
                reason: "cell ends past the page end",
            });
        }

        Ok((cell_offset, end))
    }

    /// Checks every slotted page invariant of ADR 0003.
    ///
    /// [`SlottedPage::new`] already checks the header (I1), so this checks
    /// every slot (I2), that no two live cells overlap (I3), and that the cell
    /// area is exactly the live cells plus the fragmented bytes (I4). The
    /// overlap check compares every pair of cells, so it runs in O(n²) for n
    /// cells and needs no allocation.
    ///
    /// If this returns `Ok`, [`SlottedPage::cell`] succeeds for every index
    /// below [`SlottedPage::cell_count`].
    ///
    /// # Errors
    ///
    /// Returns [`PageError::CorruptSlottedPage`] if an invariant does not hold.
    pub fn validate(&self) -> Result<(), PageError> {
        let cell_count = self.cell_count();
        let mut live_bytes: usize = 0;

        for i in 0..cell_count {
            // I2: `slot` rejects empty cells and cells outside [free_end, page_size).
            let (offset, end) = self.slot(i)?;
            live_bytes = live_bytes.checked_add(end.saturating_sub(offset)).ok_or(
                PageError::CorruptSlottedPage {
                    reason: "live cell bytes overflow",
                },
            )?;

            // I3: [a, b) and [c, d) overlap when a < d and c < b. Each pair is
            // checked once, against the cells after `i`.
            for j in i.saturating_add(1)..cell_count {
                let (other_offset, other_end) = self.slot(j)?;
                if offset < other_end && other_offset < end {
                    return Err(PageError::CorruptSlottedPage {
                        reason: "live cells overlap",
                    });
                }
            }
        }

        // I4: page_size - free_end == sum(len) + fragmented_bytes.
        let cell_area = self.page.len().saturating_sub(self.free_end());
        let accounted = live_bytes.checked_add(self.fragmented_bytes()).ok_or(
            PageError::CorruptSlottedPage {
                reason: "live plus fragmented bytes overflow",
            },
        )?;
        if cell_area != accounted {
            return Err(PageError::CorruptSlottedPage {
                reason: "cell area does not match live plus fragmented bytes",
            });
        }

        Ok(())
    }

    /// Binary searches the cells in logical order, like
    /// `slice::binary_search_by`. Returns `Ok(index)` for a match and
    /// `Err(index)` for the position where the cell would be inserted.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::CorruptSlottedPage`] if a cell cannot be read.
    pub fn search_by<F: FnMut(&[u8]) -> Ordering>(
        &self,
        mut func: F,
    ) -> Result<Result<usize, usize>, PageError> {
        let mut lo = 0;
        let mut hi = self.cell_count();

        while lo < hi {
            let mid = lo.saturating_add((hi.saturating_sub(lo)).saturating_div(2));
            match func(self.cell(mid)?) {
                Ordering::Less => lo = mid.saturating_add(1),
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Ok(Ok(mid)),
            }
        }

        Ok(Err(lo))
    }

    /// Returns the caller-owned bytes 24..32 of the header, borrowed from the
    /// page. Slotted operations never write them.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::OutOfBounds`] if the buffer is too short, which
    /// cannot happen for a view created with [`SlottedPage::new`].
    pub fn header_area(&self) -> Result<&'a [u8], PageError> {
        slice(self.page, HEADER_AREA_OFFSET, HEADER_AREA_SIZE)
    }

    /// Returns the number of cells in the page.
    #[must_use]
    pub fn cell_count(&self) -> usize {
        let count = read_u16(self.page, CELL_COUNT_OFFSET).unwrap_or(0);
        usize::from(count)
    }

    /// Returns the offset where the free space starts, just after the slot
    /// directory.
    #[must_use]
    pub fn free_start(&self) -> usize {
        (SLOT_SIZE.saturating_mul(self.cell_count())).saturating_add(SLOTTED_HEADER_SIZE)
    }

    /// Returns the offset where the cell area starts.
    #[must_use]
    pub fn free_end(&self) -> usize {
        let count = read_u32(self.page, FREE_END_OFFSET).unwrap_or(0);
        usize::try_from(count).unwrap_or(0)
    }

    /// Returns the free bytes between the slot directory and the cell area.
    #[must_use]
    pub fn contiguous_free(&self) -> usize {
        self.free_end().saturating_sub(self.free_start())
    }

    /// Returns the contiguous free bytes plus the fragmented bytes.
    #[must_use]
    pub fn total_free(&self) -> usize {
        self.contiguous_free()
            .saturating_add(self.fragmented_bytes())
    }

    /// Returns the number of dead cell bytes left by removed cells.
    #[must_use]
    pub fn fragmented_bytes(&self) -> usize {
        let count = read_u16(self.page, FRAGMENTED_BYTES_OFFSET).unwrap_or(0);
        usize::from(count)
    }
}

/// Mutable view of a slotted page, borrowing the page buffer.
#[derive(Debug)]
pub struct SlottedPageMut<'a> {
    page: &'a mut [u8],
}

impl<'a> SlottedPageMut<'a> {
    /// Formats `buffer` as an empty slotted page. Sets the page type to
    /// slotted, clears every byte from offset 5 on, and sets `free_end` to the
    /// page size. Does not touch the checksum at offset 0.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::InvalidPageSize`] if the length of `buffer` is not
    /// a valid page size, in which case `buffer` is not modified. Returns
    /// [`PageError::OutOfBounds`] if the buffer is too short for the header.
    pub fn init(buffer: &mut [u8]) -> Result<(), PageError> {
        Self::init_as(buffer, PageType::Slotted)
    }

    /// Formats `buffer` as an empty page of `page_type`, which must have a
    /// slotted layout. Like [`Self::init`], but sets the given page type.
    /// Bytes 24..32 are zeroed.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::UnexpectedPageType`] (with `expected` set to
    /// [`PageType::Slotted`]) if `page_type` has no slotted layout,
    /// [`PageError::InvalidPageSize`] if the length of `buffer` is not a valid
    /// page size, and [`PageError::OutOfBounds`] if the buffer is too short for
    /// the header. `buffer` is not modified in any of these cases.
    pub fn init_as(buffer: &mut [u8], page_type: PageType) -> Result<(), PageError> {
        if !page_type.has_slotted_layout() {
            return Err(PageError::UnexpectedPageType {
                expected: PageType::Slotted,
                found: page_type,
            });
        }
        let page_size = page_size_for(buffer)?;

        let rest = buffer.get_mut(5..).ok_or_else(|| PageError::OutOfBounds {
            offset: 5,
            len: page_size.as_usize().saturating_sub(5),
            buf_len: page_size.as_usize(),
        })?;
        rest.fill(0);
        set_page_type(buffer, page_type)?;
        write_u32(buffer, FREE_END_OFFSET, page_size.get())?;

        Ok(())
    }

    /// Creates a mutable view over `buffer`. Does not write to `buffer`.
    /// Accepts any page type with a slotted layout.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`SlottedPage::new`].
    pub fn new(buffer: &'a mut [u8]) -> Result<Self, PageError> {
        check_header(buffer)?;

        Ok(Self { page: buffer })
    }

    /// Returns a read view of the same page buffer.
    #[must_use]
    pub const fn view(&self) -> SlottedPage<'_> {
        SlottedPage { page: self.page }
    }

    /// Returns the caller-owned bytes 24..32 of the header for writing. The
    /// slotted operations never touch them, so they are the place for a
    /// sibling link or a rightmost child pointer.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::OutOfBounds`] if the buffer is too short, which
    /// cannot happen for a view created with [`SlottedPageMut::new`].
    pub fn header_area_mut(&mut self) -> Result<&mut [u8], PageError> {
        let buf_len = self.page.len();
        self.page
            .get_mut(HEADER_AREA_OFFSET..HEADER_AREA_OFFSET.saturating_add(HEADER_AREA_SIZE))
            .ok_or(PageError::OutOfBounds {
                offset: HEADER_AREA_OFFSET,
                len: HEADER_AREA_SIZE,
                buf_len,
            })
    }

    /// Inserts `cell` at logical position `index`. The cell bytes go right
    /// below `free_end` and the slots from `index` on shift one place. If the
    /// cell only fits once the fragmented bytes are reclaimed, the page is
    /// compacted first (see [`Self::compact`]). A failed insert leaves the
    /// buffer unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::EmptyCell`] for an empty cell,
    /// [`PageError::CellTooLarge`] if it exceeds [`max_cell_len`],
    /// [`PageError::SlotOutOfRange`] if `index` is past the cell count, and
    /// [`PageError::PageFull`] if the cell does not fit even after compaction.
    /// Returns [`PageError::CorruptSlottedPage`] if the page needs compaction
    /// and fails validation.
    pub fn insert(&mut self, index: usize, cell: &[u8]) -> Result<(), PageError> {
        self.insert_with(index, cell.len(), |dst| write_bytes(dst, 0, cell))
    }

    /// Inserts a cell of `len` bytes at logical position `index`, encoded in
    /// place: `write` receives the `len`-byte destination of the new cell,
    /// right below `free_end`, so the caller needs no scratch buffer.
    ///
    /// The checks, their order and the compaction rule are the same as in
    /// [`Self::insert`]. The destination is all zeros when `write` starts.
    /// Only after `write` returns `Ok` are the slots, the cell count and
    /// `free_end` updated.
    ///
    /// If `write` fails, its error is returned, the header and the slots are
    /// unchanged, and the destination is zeroed again, so the free space stays
    /// all zeros (ADR 0003, invariant 8). The page may have been compacted
    /// before `write` ran. That is fine: compaction keeps the cells and their
    /// order, so the page has the same content as before the call. If no
    /// compaction was needed, the buffer is byte-identical to the original.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::insert`], and any error returned by
    /// `write`.
    pub fn insert_with<F>(&mut self, index: usize, len: usize, write: F) -> Result<(), PageError>
    where
        F: FnOnce(&mut [u8]) -> Result<(), PageError>,
    {
        let slotted_page = self.view();
        let cell_count = slotted_page.cell_count();
        let contiguous_free = slotted_page.contiguous_free();
        let total_free = slotted_page.total_free();
        let free_start = slotted_page.free_start();

        if len == 0 {
            return Err(PageError::EmptyCell);
        }

        let max_cell_len = max_cell_len(page_size_for(self.page)?);
        if len > max_cell_len {
            return Err(PageError::CellTooLarge {
                len,
                max: max_cell_len,
            });
        }

        if index > cell_count {
            return Err(PageError::SlotOutOfRange { index, cell_count });
        }

        let needed = len.saturating_add(SLOT_SIZE);
        if needed > total_free {
            return Err(PageError::PageFull {
                needed,
                contiguous: contiguous_free,
                total_free,
            });
        }

        // The cell fits only with the fragmented bytes: compacting makes all
        // free space contiguous. It moves free_end, but not free_start.
        if needed > contiguous_free {
            self.compact()?;
        }
        let free_end = self.view().free_end();

        let new_free_end_usize = free_end.saturating_sub(len);
        let new_free_end_u32 =
            u32::try_from(new_free_end_usize).map_err(|_| PageError::CorruptSlottedPage {
                reason: "new free_end does not fit in u32",
            })?;
        let slot_pos = (index.saturating_mul(SLOT_SIZE)).saturating_add(SLOTTED_HEADER_SIZE);
        let offset_u16 =
            u16::try_from(new_free_end_u32).map_err(|_| PageError::CorruptSlottedPage {
                reason: "offset overflows",
            })?;
        let len_u16 = u16::try_from(len).map_err(|_| PageError::CorruptSlottedPage {
            reason: "len overflows",
        })?;
        let new_count = u16::try_from(cell_count.saturating_add(1)).map_err(|_| {
            PageError::CorruptSlottedPage {
                reason: "cell_count does not fit in u16",
            }
        })?;
        let slot_len_pos =
            slot_pos
                .checked_add(SLOT_LEN_OFFSET)
                .ok_or(PageError::CorruptSlottedPage {
                    reason: "slot position overflows",
                })?;
        let slots_end = free_start.saturating_add(SLOT_SIZE);

        // Check the slot directory range before `write` runs, so nothing
        // fallible is left between a successful `write` and the header update.
        if self.page.get(slot_pos..slots_end).is_none() {
            return Err(PageError::CorruptSlottedPage {
                reason: "slot directory range out of the page",
            });
        }

        // Step 1: the caller encodes the cell right below the old free_end.
        let destination = self.page.get_mut(new_free_end_usize..free_end).ok_or(
            PageError::CorruptSlottedPage {
                reason: "cell range out of the page",
            },
        )?;
        if let Err(error) = write(destination) {
            destination.fill(0);
            return Err(error);
        }

        // Step 2: shift the slots from `index` on, leaving a gap for the new one.
        let region =
            self.page
                .get_mut(slot_pos..slots_end)
                .ok_or(PageError::CorruptSlottedPage {
                    reason: "slot directory range out of the page",
                })?;
        region.copy_within(0..free_start.saturating_sub(slot_pos), SLOT_SIZE);

        // Step 3: the new slot goes in the gap left by the shift.
        write_u16(self.page, slot_pos, offset_u16)?;
        write_u16(self.page, slot_len_pos, len_u16)?;

        write_u16(self.page, CELL_COUNT_OFFSET, new_count)?;
        write_u32(self.page, FREE_END_OFFSET, new_free_end_u32)?;

        Ok(())
    }

    /// Returns the bytes of the live cell at logical position `index` for
    /// in-place changes, such as a fixed-size field. The length of the cell
    /// cannot change.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`SlottedPage::cell`].
    pub fn cell_mut(&mut self, index: usize) -> Result<&mut [u8], PageError> {
        let cell_count = self.view().cell_count();
        if index >= cell_count {
            return Err(PageError::SlotOutOfRange { index, cell_count });
        }

        let (offset, end) = self.view().slot(index)?;

        self.page
            .get_mut(offset..end)
            .ok_or(PageError::CorruptSlottedPage {
                reason: "cell not in range of page",
            })
    }

    /// Removes the cell at logical position `index`. Its bytes count as
    /// fragmented and are zeroed, along with the freed slot entry. `free_end`
    /// does not move. A failed remove leaves the buffer unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::SlotOutOfRange`] if `index` is not below the cell
    /// count, and [`PageError::CorruptSlottedPage`] if the slot at `index` is
    /// invalid.
    pub fn remove(&mut self, index: usize) -> Result<(), PageError> {
        let slotted_page = self.view();
        let cell_count = slotted_page.cell_count();
        let free_start = slotted_page.free_start();
        let fragmented_bytes = slotted_page.fragmented_bytes();

        if index >= cell_count {
            return Err(PageError::SlotOutOfRange { index, cell_count });
        }

        let (offset, end) = slotted_page.slot(index)?;
        let len = end.saturating_sub(offset);
        let new_fragmented = u16::try_from(fragmented_bytes.saturating_add(len)).map_err(|_| {
            PageError::CorruptSlottedPage {
                reason: "fragmented_bytes overflows",
            }
        })?;
        let new_count = u16::try_from(cell_count.saturating_sub(1)).map_err(|_| {
            PageError::CorruptSlottedPage {
                reason: "new_count overflows",
            }
        })?;
        let slot_pos = (index.saturating_mul(SLOT_SIZE)).saturating_add(SLOTTED_HEADER_SIZE);

        let subslice = self
            .page
            .get_mut(offset..end)
            .ok_or(PageError::CorruptSlottedPage {
                reason: "cell range out of the page",
            })?;
        subslice.fill(0);
        let region =
            self.page
                .get_mut(slot_pos..free_start)
                .ok_or(PageError::CorruptSlottedPage {
                    reason: "slot directory range out of the page",
                })?;
        let n = free_start.saturating_sub(slot_pos);
        region.copy_within(SLOT_SIZE..n, 0);
        let region_subslice = region.get_mut(n.saturating_sub(SLOT_SIZE)..n).ok_or(
            PageError::CorruptSlottedPage {
                reason: "stale slot range out of the page",
            },
        )?;
        region_subslice.fill(0);

        write_u16(self.page, CELL_COUNT_OFFSET, new_count)?;
        write_u16(self.page, FRAGMENTED_BYTES_OFFSET, new_fragmented)?;

        Ok(())
    }

    /// Rewrites the live cells in logical order from low to high addresses:
    /// cell 0 starts at the new `free_end`, each next cell starts where the
    /// previous one ends, and the last cell ends at the page end. Sets
    /// `fragmented_bytes` to 0 and zeroes the free space. The slot order and
    /// the cell contents do not change.
    ///
    /// The result depends only on the cells, so a page with no fragmented
    /// bytes is still reordered. The cells are copied to a scratch `Vec`
    /// first, because the new place of a cell can overlap a cell that has not
    /// moved yet (ADR 0004).
    ///
    /// # Errors
    ///
    /// Returns [`PageError::CorruptSlottedPage`] if the page fails
    /// [`SlottedPage::validate`]. The buffer is not modified in that case.
    pub fn compact(&mut self) -> Result<(), PageError> {
        let slotted_page = self.view();
        slotted_page.validate()?;
        let cell_count = slotted_page.cell_count();
        let free_start = slotted_page.free_start();
        let page_len = self.page.len();
        // validate checked I4, so the live bytes are the cell area minus the
        // fragmented bytes.
        let live = page_len
            .saturating_sub(slotted_page.free_end())
            .saturating_sub(slotted_page.fragmented_bytes());

        // Step 1: copy the live cells out of the page, in logical order.
        let mut scratch = Vec::with_capacity(live);
        for index in 0..cell_count {
            scratch.extend_from_slice(slotted_page.cell(index)?);
        }

        let new_free_end = page_len.saturating_sub(scratch.len());
        let new_free_end_u32 =
            u32::try_from(new_free_end).map_err(|_| PageError::CorruptSlottedPage {
                reason: "new free_end does not fit in u32",
            })?;

        // Step 2: zero the new free space, which includes the old copies of
        // the cells that sat below the new free_end.
        self.page
            .get_mut(free_start..new_free_end)
            .ok_or(PageError::CorruptSlottedPage {
                reason: "free space range out of the page",
            })?
            .fill(0);

        // Step 3: write the cells back, packed against the page end.
        write_bytes(self.page, new_free_end, &scratch)?;

        // Step 4: point each slot at the new place of its cell. Lengths do not
        // change. Every offset is below the page length, at most 65536, so it
        // fits in a u16.
        let mut offset = new_free_end;
        for index in 0..cell_count {
            let slot_pos = (index.saturating_mul(SLOT_SIZE)).saturating_add(SLOTTED_HEADER_SIZE);
            let len = read_u16(self.page, slot_pos.saturating_add(SLOT_LEN_OFFSET))?;
            let offset_u16 = u16::try_from(offset).map_err(|_| PageError::CorruptSlottedPage {
                reason: "cell offset does not fit in u16",
            })?;
            write_u16(self.page, slot_pos, offset_u16)?;
            offset = offset.saturating_add(usize::from(len));
        }

        write_u16(self.page, FRAGMENTED_BYTES_OFFSET, 0)?;
        write_u32(self.page, FREE_END_OFFSET, new_free_end_u32)?;

        Ok(())
    }
}
