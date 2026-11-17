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
//!
//! The module exposes two borrowed views over the page buffer:
//!
//! - [`SlottedPage`] reads a page through a shared `&[u8]`.
//! - [`SlottedPageMut`] reads and writes a page through a `&mut [u8]`. Its
//!   [`SlottedPageMut::view`] method returns a read view of the same buffer.

use std::cmp::Ordering;

use crate::storage::pager::{
    codec::{read_u16, read_u32, write_bytes, write_u16, write_u32},
    error::PageError,
    page::{PageType, expect_page_type, set_page_type},
    types::PageSize,
};

/// Size in bytes of the slotted page header, including the common page header.
/// The slot directory starts at this offset.
pub const SLOTTED_HEADER_SIZE: usize = 32;

/// Size in bytes of one slot directory entry: a `u16` cell offset and a `u16`
/// cell length.
pub const SLOT_SIZE: usize = 4;

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
    expect_page_type(buffer, PageType::Slotted)?;

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
    /// Creates a read view over `buffer`. Does not write to `buffer`.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::InvalidPageSize`] if `buffer` is not a valid page
    /// size, [`PageError::UnknownPageType`] or [`PageError::UnexpectedPageType`]
    /// if the page is not a slotted page, and [`PageError::CorruptSlottedPage`]
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
        let page_size = page_size_for(buffer)?;

        let rest = buffer.get_mut(5..).ok_or_else(|| PageError::OutOfBounds {
            offset: 5,
            len: page_size.as_usize().saturating_sub(5),
            buf_len: page_size.as_usize(),
        })?;
        rest.fill(0);
        set_page_type(buffer, PageType::Slotted)?;
        write_u32(buffer, FREE_END_OFFSET, page_size.get())?;

        Ok(())
    }

    /// Creates a mutable view over `buffer`. Does not write to `buffer`.
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

    /// Inserts `cell` at logical position `index`. The cell bytes go right
    /// below `free_end` and the slots from `index` on shift one place. A failed
    /// insert leaves the buffer unchanged. Fragmented space is not reused yet.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::EmptyCell`] for an empty cell,
    /// [`PageError::CellTooLarge`] if it exceeds [`max_cell_len`],
    /// [`PageError::SlotOutOfRange`] if `index` is past the cell count, and
    /// [`PageError::PageFull`] if there is no room left.
    pub fn insert(&mut self, index: usize, cell: &[u8]) -> Result<(), PageError> {
        let slotted_page = self.view();
        let cell_count = slotted_page.cell_count();
        let contiguous_free = slotted_page.contiguous_free();
        let total_free = slotted_page.total_free();
        let free_start = slotted_page.free_start();
        let free_end = slotted_page.free_end();

        if cell.is_empty() {
            return Err(PageError::EmptyCell);
        }

        let max_cell_len = max_cell_len(page_size_for(self.page)?);
        if cell.len() > max_cell_len {
            return Err(PageError::CellTooLarge {
                len: cell.len(),
                max: max_cell_len,
            });
        }

        if index > cell_count {
            return Err(PageError::SlotOutOfRange { index, cell_count });
        }

        let needed = cell.len().saturating_add(SLOT_SIZE);
        if needed > contiguous_free {
            return Err(PageError::PageFull {
                needed,
                contiguous: contiguous_free,
                total_free,
            });
        }

        let new_free_end_u32 =
            u32::try_from(free_end.saturating_sub(cell.len())).map_err(|_| {
                PageError::CorruptSlottedPage {
                    reason: "new free_end does not fit in u32",
                }
            })?;
        let new_free_end_usize = free_end.saturating_sub(cell.len());
        let slot_pos = (index.saturating_mul(SLOT_SIZE)).saturating_add(SLOTTED_HEADER_SIZE);
        let offset_u16 =
            u16::try_from(new_free_end_u32).map_err(|_| PageError::CorruptSlottedPage {
                reason: "offset overflows",
            })?;
        let len_u16 = u16::try_from(cell.len()).map_err(|_| PageError::CorruptSlottedPage {
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

        let region = self
            .page
            .get_mut(slot_pos..free_start.saturating_add(SLOT_SIZE))
            .ok_or(PageError::CorruptSlottedPage {
                reason: "slot directory range out of the page",
            })?;
        region.copy_within(0..free_start.saturating_sub(slot_pos), SLOT_SIZE);

        // Step 2: the cell bytes go right below the old free_end.
        write_bytes(self.page, new_free_end_usize, cell)?;

        // Step 3: the new slot goes in the gap left by the shift.
        write_u16(self.page, slot_pos, offset_u16)?;
        write_u16(self.page, slot_len_pos, len_u16)?;

        write_u16(self.page, CELL_COUNT_OFFSET, new_count)?;
        write_u32(self.page, FREE_END_OFFSET, new_free_end_u32)?;

        Ok(())
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
}
