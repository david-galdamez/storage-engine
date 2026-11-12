//! Free page list management.

use crate::storage::pager::{
    codec::{read_u32, write_u32},
    error::PageError,
    page::{CHECKSUM_END, PageType, page_type, set_page_type},
    types::PageId,
};

const FREE_PAGE_HEADER_END: usize = 20;
const NEXT_FREE_OFFSET: usize = 16;

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
        .ok_or(PageError::OutOfBounds {
            offset: CHECKSUM_END,
            len: buffer_len.saturating_sub(CHECKSUM_END),
            buf_len: buffer_len,
        })?;
    rest.fill(0);

    set_page_type(buffer, PageType::Free)?;
    write_u32(buffer, NEXT_FREE_OFFSET, PageId::to_disk(page_id))?;

    Ok(())
}

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
