//! Layout of a free page: one node of the free list (ADR 0003).
//!
//! Freed pages form a singly linked list used as a stack (LIFO). The file
//! header holds the head (`freelist_head`) and the length (`freelist_count`).
//! Each free page links to the next one:
//!
//! | Offset | Size | Field                                  |
//! |--------|------|----------------------------------------|
//! | 0      | 16   | common header, page type 2 (Free)      |
//! | 16     | 4    | `next_free: u32`, 0 = end of the list  |
//! | 20     | rest | zero                                   |
//!
//! These functions only read and write a buffer in memory. The pager does
//! the I/O and stamps and verifies the checksum.

use crate::storage::pager::{
    codec::{read_u32, write_u32},
    error::PageError,
    page::{CHECKSUM_END, PageType, page_type, set_page_type},
    types::PageId,
};

/// End of the free page header: the common header plus `next_free`.
const FREE_PAGE_HEADER_END: usize = 20;
/// Offset of the `next_free` field.
const NEXT_FREE_OFFSET: usize = 16;

/// Turns `buffer` into a free page that links to `page_id`, the next free
/// page (`None` ends the list).
///
/// Zeroes every byte from offset 4 to the end (so the LSN is 0), sets the
/// page type to [`PageType::Free`], and writes `next_free` at offset 16.
/// Never touches the checksum `[0, 4)`: the pager stamps it on write.
///
/// # Errors
///
/// - [`PageError::OutOfBounds`] if `buffer` is shorter than 20 bytes. The
///   buffer is not modified.
pub fn encode_free_page(buffer: &mut [u8], page_id: Option<PageId>) -> Result<(), PageError> {
    let buffer_len = buffer.len();

    if buffer_len < FREE_PAGE_HEADER_END {
        return Err(PageError::OutOfBounds {
            offset: 0,
            len: FREE_PAGE_HEADER_END,
            buf_len: buffer.len(),
        });
    }

    let rest = buffer
        .get_mut(CHECKSUM_END..)
        .ok_or_else(|| PageError::OutOfBounds {
            offset: CHECKSUM_END,
            len: buffer_len.saturating_sub(CHECKSUM_END),
            buf_len: buffer_len,
        })?;
    rest.fill(0);

    set_page_type(buffer, PageType::Free)?;
    write_u32(buffer, NEXT_FREE_OFFSET, PageId::to_disk(page_id))?;

    Ok(())
}

/// Reads a free page and returns its `next_free` link (`None` at the end of
/// the list). Does not verify the checksum: the pager does that on read.
///
/// # Errors
///
/// - [`PageError::OutOfBounds`] if `buffer` is shorter than 20 bytes.
/// - [`PageError::UnknownPageType`] if the page type byte is not a known
///   [`PageType`].
/// - [`PageError::UnexpectedPageType`] if the page is valid but not a free
///   page.
pub fn decode_free_page(buffer: &[u8]) -> Result<Option<PageId>, PageError> {
    if buffer.len() < FREE_PAGE_HEADER_END {
        return Err(PageError::OutOfBounds {
            offset: 0,
            len: FREE_PAGE_HEADER_END,
            buf_len: buffer.len(),
        });
    }

    let page_type = page_type(buffer)?;
    if page_type != PageType::Free {
        return Err(PageError::UnexpectedPageType {
            expected: PageType::Free,
            found: page_type,
        });
    }

    let page_id = read_u32(buffer, NEXT_FREE_OFFSET)?;

    Ok(PageId::from_disk(page_id))
}
