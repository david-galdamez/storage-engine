//! Database file header: magic number, format version, and page size.
//!
//! Stored in page 0, after the common page header (ADR 0003):
//!
//! | Offset | Size | Field                                                       |
//! |--------|------|-------------------------------------------------------------|
//! | 16     | 8    | magic: `STENGDB\0`                                          |
//! | 24     | 2    | `format_version`: u16                                       |
//! | 26     | 2    | reserved                                                    |
//! | 28     | 4    | `page_size`: u32                                            |
//! | 32     | 4    | `page_count`: u32, >= 1, includes page 0                    |
//! | 36     | 4    | `freelist_head`: u32, 0 = empty, otherwise < `page_count`    |
//! | 40     | 4    | `freelist_count`: u32, 0 if and only if head is 0, < `page_count` |
//! | 44     | rest | reserved                                                    |

use crate::storage::pager::{
    codec::{read_u16, read_u32, slice, write_bytes, write_u16, write_u32},
    error::PageError,
    page::{CHECKSUM_END, PageType, expect_page_type, set_page_type},
    types::{PageId, PageSize},
};

/// Identifies a database file of this engine: ASCII `STENGDB` and a zero byte.
pub const MAGIC: [u8; 8] = *b"STENGDB\0";
/// On-disk format version written by this build. Any layout change bumps it.
pub const FORMAT_VERSION: u16 = 1;
/// First byte after the file header fields. `decode` needs at least this many bytes.
pub const FILE_HEADER_END: usize = 44;

const OFF_MAGIC: usize = 16;
const OFF_VERSION: usize = 24;
const OFF_PAGE_SIZE: usize = 28;
const OFF_PAGE_COUNT: usize = 32;
const OFF_FREELIST_HEAD: usize = 36;
const OFF_FREELIST_COUNT: usize = 40;

/// Contents of the database file header (page 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHeader {
    /// Size of every page in the file, fixed when the file is created.
    pub page_size: PageSize,
    /// Number of pages in the file, including page 0.
    pub page_count: u32,
    /// First page of the free list, or `None` if no page is free.
    pub freelist_head: Option<PageId>,
    /// Number of pages in the free list.
    pub freelist_count: u32,
}

impl FileHeader {
    /// Returns the header of a new database: only page 0 and an empty free list.
    #[must_use]
    pub const fn new(page_size: PageSize) -> Self {
        Self {
            page_size,
            page_count: 1,
            freelist_head: None,
            freelist_count: 0,
        }
    }

    /// Writes the header into `buffer`.
    ///
    /// Zeroes every byte from offset 4 to the end of the buffer, then writes the
    /// page type and the fields. The LSN stays 0. Never touches the checksum
    /// `[0, 4)`: the pager stamps it when the page is written.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::OutOfBounds`] if `buffer` is shorter than
    /// [`FILE_HEADER_END`]. The buffer is not modified in that case.
    pub fn encode(&self, buffer: &mut [u8]) -> Result<(), PageError> {
        slice(buffer, 0, FILE_HEADER_END)?;
        let buffer_len = buffer.len();

        let rest = buffer
            .get_mut(CHECKSUM_END..)
            .ok_or_else(|| PageError::OutOfBounds {
                offset: CHECKSUM_END,
                len: buffer_len.saturating_sub(CHECKSUM_END),
                buf_len: buffer_len,
            })?;

        rest.fill(0);

        set_page_type(buffer, PageType::FileHeader)?;
        write_bytes(buffer, OFF_MAGIC, &MAGIC)?;
        write_u16(buffer, OFF_VERSION, FORMAT_VERSION)?;
        write_u32(buffer, OFF_PAGE_SIZE, self.page_size.get())?;
        write_u32(buffer, OFF_PAGE_COUNT, self.page_count)?;
        write_u32(
            buffer,
            OFF_FREELIST_HEAD,
            PageId::to_disk(self.freelist_head),
        )?;
        write_u32(buffer, OFF_FREELIST_COUNT, self.freelist_count)?;

        Ok(())
    }

    /// Reads and validates the header from `buffer`.
    ///
    /// Works on any prefix of page 0 of at least [`FILE_HEADER_END`] bytes, so
    /// the pager can call it before it knows the page size. Does not verify the
    /// checksum, and ignores the LSN and the reserved bytes.
    ///
    /// # Errors
    ///
    /// Checks in this order and returns the first failure:
    ///
    /// 1. [`PageError::OutOfBounds`] if `buffer` is shorter than [`FILE_HEADER_END`].
    /// 2. [`PageError::BadMagic`] if the magic number does not match.
    /// 3. [`PageError::UnsupportedVersion`] if the format version is not [`FORMAT_VERSION`].
    /// 4. [`PageError::InvalidPageSize`] if the page size is not valid (ADR 0001).
    /// 5. [`PageError::UnknownPageType`] or [`PageError::UnexpectedPageType`] if
    ///    page 0 is not a file header page.
    /// 6. [`PageError::CorruptHeader`] if the page count and free list fields
    ///    contradict each other.
    pub fn decode(buffer: &[u8]) -> Result<Self, PageError> {
        let buffer = slice(buffer, 0, FILE_HEADER_END)?;

        let magic = slice(buffer, OFF_MAGIC, MAGIC.len())?;
        if magic != MAGIC {
            return Err(PageError::BadMagic {
                found: <[u8; MAGIC.len()]>::try_from(magic).map_err(|_| {
                    PageError::OutOfBounds {
                        len: MAGIC.len(),
                        offset: OFF_MAGIC,
                        buf_len: buffer.len(),
                    }
                })?,
            });
        }

        let version = read_u16(buffer, OFF_VERSION)?;
        if version != FORMAT_VERSION {
            return Err(PageError::UnsupportedVersion { version });
        }

        let file_header_page_size = read_u32(buffer, OFF_PAGE_SIZE)?;
        let page_size = PageSize::new(file_header_page_size)?;
        expect_page_type(buffer, PageType::FileHeader)?;
        let page_count = read_u32(buffer, OFF_PAGE_COUNT)?;

        let file_header_free_list_head = read_u32(buffer, OFF_FREELIST_HEAD)?;
        let freelist_head = PageId::from_disk(file_header_free_list_head);
        let freelist_count = read_u32(buffer, OFF_FREELIST_COUNT)?;

        if page_count == 0 {
            return Err(PageError::CorruptHeader {
                reason: "page_count is 0",
            });
        }

        if file_header_free_list_head == 0 {
            if freelist_count != 0 {
                return Err(PageError::CorruptHeader {
                    reason: "free list is empty but freelist_count is not 0",
                });
            }
        } else {
            if file_header_free_list_head >= page_count {
                return Err(PageError::CorruptHeader {
                    reason: "freelist_head is past the end of the file",
                });
            }

            if freelist_count == 0 {
                return Err(PageError::CorruptHeader {
                    reason: "freelist_head is set but freelist_count is 0",
                });
            }
            if freelist_count >= page_count {
                return Err(PageError::CorruptHeader {
                    reason: "freelist_count exceeds the pages that can be free",
                });
            }
        }

        Ok(Self {
            page_size,
            page_count,
            freelist_head,
            freelist_count,
        })
    }
}
