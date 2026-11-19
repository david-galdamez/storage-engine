//! Tests for the slotted page (`storage::pager::slotted`, steps 8 and 9).
//!
//! The byte layout is documented in ADR 0003. These tests check:
//!
//! - `init` writes the ADR layout (type byte, empty counters, `free_end` as a
//!   u32), zeroes the body and the LSN, and leaves the checksum alone. `init`
//!   and `new` reject buffers that are not a valid page size, and `new`
//!   rejects pages of another type.
//! - `insert` writes the slot and the cell bytes at literal offsets, keeps the
//!   logical order chosen by the caller, and never touches the checksum, the
//!   common header, or the flags and reserved bytes.
//! - Size limits: the largest cell fits exactly, and the errors come in a fixed
//!   order (empty cell, cell too large, slot out of range, page full). A failed
//!   insert or remove leaves the buffer byte-for-byte unchanged.
//! - `remove` shifts the slots, adds the cell length to `fragmented_bytes`,
//!   zeroes the dead bytes and the stale slot, and never moves `free_end`.
//! - `compact` (step 9) rewrites the live cells in logical order from low to
//!   high addresses: cell 0 starts at the new `free_end` and the last cell
//!   ends at the page end, with no holes. It updates the slot offsets, sets
//!   `fragmented_bytes` to 0, zeroes the rest of the page body, and keeps the
//!   slot order and cell contents. The result depends only on the cells, not
//!   on the history of the page, so `compact` reorders cells even when
//!   `fragmented_bytes` is 0. It validates the whole page first and writes
//!   nothing on a corrupt page.
//! - `insert` compacts first only when the cell does not fit in the contiguous
//!   free space but fits in the total free space. Then the new cell goes to
//!   `free_end` as usual. `PageFull` now means the cell does not fit even
//!   after compaction.
//! - The free gap `[free_start, free_end)` is always all zeros.
//! - `search_by` behaves like `slice::binary_search_by`.
//! - Corrupted pages (bad header fields, bad slots, a sum mismatch) are always
//!   an `Err`, never a panic, and mutations on them write nothing.
//! - Every operation keeps the invariants I1 to I4 of ADR 0003. They are
//!   checked from the raw bytes by `check_invariants`, not through the API.
//!   Property tests check random bytes (no panics), random inserts against a
//!   `Vec` model, and random inserts, removes and compactions against a model
//!   that predicts which inserts must fit.
//! - Model tests (step 10): the page used as a sorted B+Tree leaf behaves like
//!   a `BTreeMap` (search ranks, free space, `PageFull` leaves the buffer
//!   unchanged), and `compact` gives the same bytes for the same cells
//!   whatever the history that produced them.
//!
//! Expected values do not come from the code under test: offsets and encoded
//! bytes are written as literals (little-endian).
//!
//! # API under test
//!
//! Two borrowed views over the page buffer: `SlottedPage<'a>` (read) and
//! `SlottedPageMut<'a>` (write). The behavior is defined by the tests below.
//!
//! ```text
//! SLOTTED_HEADER_SIZE, SLOT_SIZE
//! max_cell_len(PageSize) -> usize
//!
//! SlottedPage::new(&[u8]) -> Result<Self, PageError>
//!   cell_count, fragmented_bytes, free_start, free_end,
//!   contiguous_free, total_free                     -> usize
//!   cell(usize)                                     -> Result<&[u8], PageError>
//!   search_by(FnMut(&[u8]) -> Ordering)             -> Result<Result<usize, usize>, PageError>
//!   validate()                                      -> Result<(), PageError>
//!
//! SlottedPageMut::init(&mut [u8]) / new(&mut [u8])  -> Result<_, PageError>
//!   view()                                          -> SlottedPage<'_>
//!   insert(usize, &[u8]) / remove(usize)            -> Result<(), PageError>
//!   compact()                                       -> Result<(), PageError>
//!
//! New PageError variants: EmptyCell, CellTooLarge { len, max },
//! SlotOutOfRange { index, cell_count }, PageFull { needed, contiguous,
//! total_free }, CorruptSlottedPage { .. }
//! ```

mod common;

use std::cmp::Ordering;
use std::collections::BTreeMap;

use anyhow::{Context, ensure};
use proptest::prelude::*;
use proptest::sample::Index;
use storage_engine_rs::storage::pager::error::PageError;
use storage_engine_rs::storage::pager::freelist::encode_free_page;
use storage_engine_rs::storage::pager::page::PageType;
use storage_engine_rs::storage::pager::slotted::{
    SLOT_SIZE, SLOTTED_HEADER_SIZE, SlottedPage, SlottedPageMut, max_cell_len,
};
use storage_engine_rs::storage::pager::types::PageSize;

use common::{
    add, cells_of, check_compacted, check_free_gap_zeroed, check_invariants, check_no_holes,
    count_of, empty_page, fragmented_of, free_end_of, free_start_of, mul, slice, sub,
};

/// Page sizes allowed by ADR 0001 that the tests cover: smallest, default, largest.
const SIZES: [usize; 3] = [512, 4096, 65536];

// Offsets written here independently of the code (ADR 0003).
const OFF_CELL_COUNT: usize = 16;
const OFF_FRAGMENTED: usize = 18;
const OFF_FREE_END: usize = 20;
const OFF_SLOTS: usize = 32;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Overwrites bytes of `buf` starting at `at`.
fn put(buf: &mut [u8], at: usize, bytes: &[u8]) -> anyhow::Result<()> {
    let end = add(at, bytes.len())?;
    buf.get_mut(at..end)
        .context("range past buffer")?
        .copy_from_slice(bytes);
    Ok(())
}

fn put_u16(buf: &mut [u8], at: usize, value: u16) -> anyhow::Result<()> {
    put(buf, at, &value.to_le_bytes())
}

fn put_u32(buf: &mut [u8], at: usize, value: u32) -> anyhow::Result<()> {
    put(buf, at, &value.to_le_bytes())
}

/// Sets slot `index` to `[offset, len]` directly in the raw bytes.
fn put_slot(buf: &mut [u8], index: usize, offset: u16, len: u16) -> anyhow::Result<()> {
    let at = add(OFF_SLOTS, mul(4, index)?)?;
    put_u16(buf, at, offset)?;
    put_u16(buf, add(at, 2)?, len)
}

fn insert(buf: &mut [u8], index: usize, cell: &[u8]) -> Result<(), PageError> {
    SlottedPageMut::new(buf)?.insert(index, cell)
}

fn remove(buf: &mut [u8], index: usize) -> Result<(), PageError> {
    SlottedPageMut::new(buf)?.remove(index)
}

fn compact(buf: &mut [u8]) -> Result<(), PageError> {
    SlottedPageMut::new(buf)?.compact()
}

/// `compact` on a view, for `mut_view_rejects`.
fn compact_view(page: &mut SlottedPageMut<'_>) -> Result<(), PageError> {
    page.compact()
}

/// A 512-byte page with the cells `ab` (index 0) and `hello` (index 1).
///
/// Layout: hello at 507..512, ab at 505..507, slot 0 = [505, 2] at 32..36,
/// slot 1 = [507, 5] at 36..40, `free_end` = 505.
fn two_cell_page() -> anyhow::Result<Vec<u8>> {
    let mut buf = empty_page(512)?;
    insert(&mut buf, 0, b"hello")?;
    insert(&mut buf, 0, b"ab")?;
    check_invariants(&buf)?;
    Ok(buf)
}

/// A 512-byte page with the live cells `hello` (index 0) and `ab` (index 1)
/// and a 400-byte hole between them.
///
/// Built by inserting hello (507..512), a 400-byte cell (107..507) and ab
/// (105..107), then removing the 400-byte cell. Slot 0 = [507, 5], slot 1 =
/// [105, 2], `free_start` = 40, `free_end` = 105, `fragmented_bytes` = 400,
/// `contiguous_free` = 65, `total_free` = 465.
fn fragmented_page() -> anyhow::Result<Vec<u8>> {
    let mut buf = empty_page(512)?;
    insert(&mut buf, 0, b"hello")?;
    insert(&mut buf, 1, &[0x33; 400])?;
    insert(&mut buf, 2, b"ab")?;
    remove(&mut buf, 1)?;
    check_invariants(&buf)?;
    ensure!(fragmented_of(&buf)? == 400, "fixture: fragmented_bytes");
    ensure!(free_end_of(&buf)? == 105, "fixture: free_end");
    Ok(buf)
}

/// The bytes a slotted page must never change on insert and remove: the
/// common header `[0, 16)` and the flags and reserved bytes `[24, 32)`.
fn protected_bytes(buf: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut bytes = slice(buf, 0, 16)?.to_vec();
    bytes.extend_from_slice(slice(buf, 24, 8)?);
    Ok(bytes)
}

fn expect_corrupt<T>(result: Result<T, PageError>, what: &str) -> anyhow::Result<()> {
    let error = result.err();
    ensure!(
        matches!(error, Some(PageError::CorruptSlottedPage { .. })),
        "{what}: expected CorruptSlottedPage, got {error:?}"
    );
    Ok(())
}

/// `SlottedPage::new` followed by `validate` must fail with `CorruptSlottedPage`.
/// Either step may be the one that fails.
fn validate_is_corrupt(buf: &[u8], what: &str) -> anyhow::Result<()> {
    expect_corrupt(SlottedPage::new(buf).and_then(|page| page.validate()), what)
}

/// `SlottedPage::new` followed by `op` must fail with `CorruptSlottedPage`.
/// Either step may be the one that fails.
fn read_view_rejects(
    buf: &[u8],
    what: &str,
    op: impl FnOnce(&SlottedPage<'_>) -> Result<(), PageError>,
) -> anyhow::Result<()> {
    expect_corrupt(SlottedPage::new(buf).and_then(|page| op(&page)), what)
}

/// `SlottedPageMut::new` followed by `op` must fail with `CorruptSlottedPage`
/// and leave the buffer unchanged. Either step may be the one that fails.
fn mut_view_rejects(
    buf: &[u8],
    what: &str,
    op: impl FnOnce(&mut SlottedPageMut<'_>) -> Result<(), PageError>,
) -> anyhow::Result<()> {
    let mut copy = buf.to_vec();
    let outcome = SlottedPageMut::new(&mut copy).and_then(|mut page| op(&mut page));
    expect_corrupt(outcome, what)?;
    ensure!(copy == buf, "{what}: the buffer changed");
    Ok(())
}

// ---------------------------------------------------------------------------
// Layout and init
// ---------------------------------------------------------------------------

#[test]
fn init_writes_the_adr_layout() -> anyhow::Result<()> {
    let buf = empty_page(512)?;
    ensure!(buf.get(4) == Some(&3), "type byte is {:?}", buf.get(4));
    ensure!(
        slice(&buf, 16, 4)? == [0, 0, 0, 0],
        "cell_count or fragmented"
    );
    ensure!(slice(&buf, 20, 4)? == [0x00, 0x02, 0x00, 0x00], "free_end");
    ensure!(slice(&buf, 24, 8)? == [0; 8], "flags and reserved");
    ensure!(
        slice(&buf, 0, 4)? == [0xAA; 4],
        "checksum bytes were touched"
    );
    check_invariants(&buf)
}

#[test]
fn init_free_end_is_u32_for_64k_pages() -> anyhow::Result<()> {
    let buf = empty_page(65536)?;
    ensure!(
        slice(&buf, 20, 4)? == [0x00, 0x00, 0x01, 0x00],
        "free_end bytes are {:?}",
        slice(&buf, 20, 4)?
    );
    ensure!(
        slice(&buf, 16, 4)? == [0, 0, 0, 0],
        "cell_count or fragmented"
    );
    check_invariants(&buf)
}

#[test]
fn init_zeroes_body_and_lsn() -> anyhow::Result<()> {
    for size in SIZES {
        let buf = empty_page(size)?;
        ensure!(
            slice(&buf, 5, 11)? == [0; 11],
            "size {size}: [5, 16) not zero"
        );
        ensure!(slice(&buf, 8, 8)? == [0; 8], "size {size}: LSN not zero");
        let body = slice(&buf, 32, sub(size, 32)?)?;
        ensure!(body.iter().all(|&b| b == 0), "size {size}: body not zero");
        ensure!(
            slice(&buf, 0, 4)? == [0xAA; 4],
            "size {size}: checksum touched"
        );
        let view = SlottedPage::new(&buf)?;
        ensure!(view.cell_count() == 0, "size {size}: cell_count");
        ensure!(view.fragmented_bytes() == 0, "size {size}: fragmented");
        ensure!(view.free_start() == 32, "size {size}: free_start");
        ensure!(view.free_end() == size, "size {size}: free_end");
        ensure!(
            view.contiguous_free() == sub(size, 32)?,
            "size {size}: contiguous"
        );
        ensure!(
            view.total_free() == sub(size, 32)?,
            "size {size}: total_free"
        );
        view.validate()?;
    }
    Ok(())
}

#[test]
fn init_rejects_invalid_buffer_lengths() -> anyhow::Result<()> {
    for len in [0usize, 31, 100, 513] {
        let mut buf = vec![0xAA; len];
        let error = SlottedPageMut::init(&mut buf).err();
        ensure!(
            matches!(error, Some(PageError::InvalidPageSize { .. })),
            "init with len {len}: {error:?}"
        );
        ensure!(
            buf.iter().all(|&b| b == 0xAA),
            "init with len {len} modified the buffer"
        );

        let mut buf = vec![0xAA; len];
        let error = SlottedPageMut::new(&mut buf).err();
        ensure!(
            matches!(error, Some(PageError::InvalidPageSize { .. })),
            "new (mut) with len {len}: {error:?}"
        );
        let buf = vec![0xAA; len];
        let error = SlottedPage::new(&buf).err();
        ensure!(
            matches!(error, Some(PageError::InvalidPageSize { .. })),
            "new (read) with len {len}: {error:?}"
        );
    }
    Ok(())
}

#[test]
fn new_rejects_wrong_page_type() -> anyhow::Result<()> {
    let mut free = vec![0u8; 512];
    encode_free_page(&mut free, None)?;
    let expected = PageError::UnexpectedPageType {
        expected: PageType::Slotted,
        found: PageType::Free,
    };
    ensure!(
        SlottedPage::new(&free).err() == Some(expected),
        "read view, free page"
    );
    let mut copy = free.clone();
    ensure!(
        SlottedPageMut::new(&mut copy).err() == Some(expected),
        "mut view, free page"
    );
    ensure!(copy == free, "new modified a free page");

    let mut zeroed = vec![0u8; 512];
    let expected = PageError::UnknownPageType { value: 0 };
    ensure!(
        SlottedPage::new(&zeroed).err() == Some(expected),
        "read view, zeroed"
    );
    ensure!(
        SlottedPageMut::new(&mut zeroed).err() == Some(expected),
        "mut view, zeroed"
    );
    Ok(())
}

#[test]
fn constants_and_max_cell_len() -> anyhow::Result<()> {
    ensure!(SLOTTED_HEADER_SIZE == 32, "header size");
    ensure!(SLOT_SIZE == 4, "slot size");
    for (size, expected) in [(512u32, 476usize), (4096, 4060), (65536, 65500)] {
        let got = max_cell_len(PageSize::new(size)?);
        ensure!(
            got == expected,
            "size {size}: max_cell_len {got}, expected {expected}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Insert and read
// ---------------------------------------------------------------------------

#[test]
fn insert_one_cell_literal_bytes() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    insert(&mut buf, 0, b"hello")?;
    ensure!(slice(&buf, 507, 5)? == *b"hello", "cell bytes at 507..512");
    // slot 0 = [offset 507 = 0x01FB, len 5]
    ensure!(slice(&buf, 32, 4)? == [0xFB, 0x01, 0x05, 0x00], "slot 0");
    ensure!(slice(&buf, 16, 2)? == [0x01, 0x00], "cell_count");
    ensure!(slice(&buf, 18, 2)? == [0x00, 0x00], "fragmented_bytes");
    ensure!(slice(&buf, 20, 4)? == [0xFB, 0x01, 0x00, 0x00], "free_end");
    let page = SlottedPage::new(&buf)?;
    ensure!(page.cell(0)? == b"hello", "cell(0)");
    ensure!(page.cell_count() == 1, "cell_count()");
    ensure!(page.free_start() == 36, "free_start");
    ensure!(page.free_end() == 507, "free_end");
    ensure!(page.contiguous_free() == 471, "contiguous_free");
    ensure!(page.total_free() == 471, "total_free");
    page.validate()?;
    check_invariants(&buf)
}

#[test]
fn insert_at_front_keeps_logical_order() -> anyhow::Result<()> {
    let buf = two_cell_page()?;
    // "ab" is stored at 505..507, offset 505 = 0x01F9. It is logical cell 0.
    ensure!(slice(&buf, 505, 2)? == *b"ab", "ab bytes");
    ensure!(slice(&buf, 32, 4)? == [0xF9, 0x01, 0x02, 0x00], "slot 0");
    ensure!(slice(&buf, 36, 4)? == [0xFB, 0x01, 0x05, 0x00], "slot 1");
    ensure!(slice(&buf, 20, 4)? == [0xF9, 0x01, 0x00, 0x00], "free_end");
    let page = SlottedPage::new(&buf)?;
    ensure!(page.cell(0)? == b"ab", "cell(0)");
    ensure!(page.cell(1)? == b"hello", "cell(1)");
    ensure!(page.cell_count() == 2, "cell_count");
    ensure!(page.free_start() == 40, "free_start");
    ensure!(page.free_end() == 505, "free_end");
    ensure!(page.contiguous_free() == 465, "contiguous_free");
    Ok(())
}

#[test]
fn insert_in_middle_and_append() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    insert(&mut buf, 0, b"a")?;
    insert(&mut buf, 1, b"ccc")?;
    check_invariants(&buf)?;
    insert(&mut buf, 1, b"bb")?;
    check_invariants(&buf)?;
    insert(&mut buf, 3, b"dddd")?;
    check_invariants(&buf)?;
    insert(&mut buf, 0, b"z")?;
    check_invariants(&buf)?;
    let expected: Vec<Vec<u8>> = vec![
        b"z".to_vec(),
        b"a".to_vec(),
        b"bb".to_vec(),
        b"ccc".to_vec(),
        b"dddd".to_vec(),
    ];
    ensure!(
        cells_of(&buf)? == expected,
        "cells are {:?}",
        cells_of(&buf)?
    );
    Ok(())
}

#[test]
fn insert_does_not_touch_checksum_common_header_or_flags() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    put(&mut buf, 0, &[1, 2, 3, 4])?;
    put(&mut buf, 5, &[0x11; 11])?;
    put(
        &mut buf,
        24,
        &[0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87],
    )?;
    let before = protected_bytes(&buf)?;
    ensure!(buf.get(4) == Some(&3), "type byte lost");

    insert(&mut buf, 0, b"one")?;
    ensure!(protected_bytes(&buf)? == before, "after first insert");
    insert(&mut buf, 1, b"two")?;
    insert(&mut buf, 0, b"three")?;
    ensure!(protected_bytes(&buf)? == before, "after more inserts");
    remove(&mut buf, 1)?;
    ensure!(protected_bytes(&buf)? == before, "after remove");
    remove(&mut buf, 0)?;
    remove(&mut buf, 0)?;
    ensure!(
        protected_bytes(&buf)? == before,
        "after removing everything"
    );
    check_invariants(&buf)
}

#[test]
fn cell_returns_borrowed_slice_from_page() -> anyhow::Result<()> {
    let buf = two_cell_page()?;
    let range = buf.as_ptr_range();
    let page = SlottedPage::new(&buf)?;
    for index in 0..2 {
        let cell = page.cell(index)?;
        let cell_range = cell.as_ptr_range();
        ensure!(
            cell_range.start >= range.start && cell_range.end <= range.end,
            "cell {index} does not point into the page buffer"
        );
    }
    Ok(())
}

#[test]
fn max_cell_fits_exactly() -> anyhow::Result<()> {
    for size in SIZES {
        let max = sub(size, 36)?;
        let cell = vec![0x5A; max];
        let mut buf = empty_page(size)?;
        insert(&mut buf, 0, &cell)?;
        check_invariants(&buf)?;
        let page = SlottedPage::new(&buf)?;
        ensure!(
            page.free_end() == 36,
            "size {size}: free_end {}",
            page.free_end()
        );
        ensure!(page.free_start() == 36, "size {size}: free_start");
        ensure!(page.contiguous_free() == 0, "size {size}: contiguous_free");
        ensure!(page.total_free() == 0, "size {size}: total_free");
        ensure!(page.cell(0)? == cell.as_slice(), "size {size}: cell bytes");
    }
    Ok(())
}

#[test]
fn cell_too_large_is_rejected() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    let error = insert(&mut buf, 0, &[7; 477]).err();
    ensure!(
        error == Some(PageError::CellTooLarge { len: 477, max: 476 }),
        "got {error:?}"
    );
    for size in SIZES {
        let max = sub(size, 36)?;
        let mut buf = empty_page(size)?;
        let error = insert(&mut buf, 0, &vec![7; add(max, 1)?]).err();
        ensure!(
            error
                == Some(PageError::CellTooLarge {
                    len: add(max, 1)?,
                    max
                }),
            "size {size}: got {error:?}"
        );
    }
    Ok(())
}

#[test]
fn empty_cell_is_rejected() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    let error = insert(&mut buf, 0, &[]).err();
    ensure!(error == Some(PageError::EmptyCell), "got {error:?}");
    Ok(())
}

#[test]
fn insert_errors_are_checked_in_a_fixed_order() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    // EmptyCell comes before SlotOutOfRange.
    ensure!(
        insert(&mut buf, 5, &[]).err() == Some(PageError::EmptyCell),
        "empty cell at a bad index"
    );
    // CellTooLarge comes before SlotOutOfRange.
    ensure!(
        insert(&mut buf, 5, &[1; 477]).err()
            == Some(PageError::CellTooLarge { len: 477, max: 476 }),
        "too large at a bad index"
    );
    // SlotOutOfRange comes before PageFull.
    for i in 0..96 {
        insert(&mut buf, i, &[0x41])?;
    }
    let error = insert(&mut buf, 97, &[0x42]).err();
    ensure!(
        error
            == Some(PageError::SlotOutOfRange {
                index: 97,
                cell_count: 96
            }),
        "full page at a bad index: {error:?}"
    );
    Ok(())
}

#[test]
fn page_full_after_96_one_byte_cells() -> anyhow::Result<()> {
    // 512 - 32 = 480 bytes of room, 5 bytes per cell (1 byte + 4 byte slot).
    let mut buf = empty_page(512)?;
    for i in 0..96 {
        insert(&mut buf, i, &[0x41])?;
        check_invariants(&buf)?;
    }
    let page = SlottedPage::new(&buf)?;
    ensure!(page.cell_count() == 96, "cell_count");
    ensure!(page.contiguous_free() == 0, "contiguous_free");
    ensure!(
        page.free_start() == 416 && page.free_end() == 416,
        "free_start/free_end"
    );

    let error = insert(&mut buf, 96, &[0x42]).err();
    ensure!(
        error
            == Some(PageError::PageFull {
                needed: 5,
                contiguous: 0,
                total_free: 0
            }),
        "got {error:?}"
    );
    Ok(())
}

#[test]
fn insert_index_past_count_is_rejected() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    let error = insert(&mut buf, 1, b"x").err();
    ensure!(
        error
            == Some(PageError::SlotOutOfRange {
                index: 1,
                cell_count: 0
            }),
        "empty page: {error:?}"
    );
    let mut buf = two_cell_page()?;
    let error = insert(&mut buf, 3, b"x").err();
    ensure!(
        error
            == Some(PageError::SlotOutOfRange {
                index: 3,
                cell_count: 2
            }),
        "two cells: {error:?}"
    );
    // index == cell_count is an append, not an error.
    insert(&mut buf, 2, b"x")?;
    check_invariants(&buf)
}

#[test]
fn failed_insert_leaves_buffer_unchanged() -> anyhow::Result<()> {
    let mut buf = two_cell_page()?;
    let before = buf.clone();
    ensure!(insert(&mut buf, 0, &[]).is_err(), "empty cell");
    ensure!(insert(&mut buf, 0, &[1; 477]).is_err(), "too large");
    ensure!(insert(&mut buf, 3, b"x").is_err(), "index past count");
    ensure!(insert(&mut buf, 0, &[1; 470]).is_err(), "page full");
    ensure!(buf == before, "a failed insert changed the buffer");

    let mut full = empty_page(512)?;
    for i in 0..96 {
        insert(&mut full, i, &[0x41])?;
    }
    let before = full.clone();
    ensure!(insert(&mut full, 0, b"x").is_err(), "full page");
    ensure!(
        full == before,
        "a failed insert on a full page changed the buffer"
    );
    Ok(())
}

#[test]
fn cell_index_out_of_range_is_rejected() -> anyhow::Result<()> {
    let buf = empty_page(512)?;
    let page = SlottedPage::new(&buf)?;
    ensure!(
        page.cell(0).err()
            == Some(PageError::SlotOutOfRange {
                index: 0,
                cell_count: 0
            }),
        "empty page"
    );
    let buf = two_cell_page()?;
    let page = SlottedPage::new(&buf)?;
    ensure!(
        page.cell(2).err()
            == Some(PageError::SlotOutOfRange {
                index: 2,
                cell_count: 2
            }),
        "index == count"
    );
    ensure!(
        page.cell(usize::MAX).err()
            == Some(PageError::SlotOutOfRange {
                index: usize::MAX,
                cell_count: 2
            }),
        "usize::MAX"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Remove
// ---------------------------------------------------------------------------

#[test]
fn remove_shifts_slots_and_counts_fragmented_bytes() -> anyhow::Result<()> {
    let mut buf = two_cell_page()?;
    remove(&mut buf, 0)?;
    check_invariants(&buf)?;
    ensure!(count_of(&buf)? == 1, "cell_count");
    ensure!(
        slice(&buf, 32, 4)? == [0xFB, 0x01, 0x05, 0x00],
        "slot 0 is now hello"
    );
    ensure!(fragmented_of(&buf)? == 2, "fragmented_bytes");
    // Removing never moves free_end: it stays at 505.
    ensure!(free_end_of(&buf)? == 505, "free_end");
    let page = SlottedPage::new(&buf)?;
    ensure!(page.cell(0)? == b"hello", "cell(0)");
    ensure!(page.cell_count() == 1, "cell_count()");
    ensure!(page.fragmented_bytes() == 2, "fragmented_bytes()");
    ensure!(page.free_start() == 36, "free_start");
    ensure!(page.free_end() == 505, "free_end()");
    ensure!(page.contiguous_free() == 469, "contiguous_free");
    ensure!(page.total_free() == 471, "total_free");
    Ok(())
}

#[test]
fn remove_zeroes_dead_bytes_and_stale_slot() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    insert(&mut buf, 0, b"aaa")?; // 509..512, offset 0x01FD
    insert(&mut buf, 1, b"bbbb")?; // 505..509, offset 0x01F9
    insert(&mut buf, 2, b"cc")?; // 503..505, offset 0x01F7
    ensure!(
        slice(&buf, 40, 4)? == [0xF7, 0x01, 0x02, 0x00],
        "slot 2 before remove"
    );

    remove(&mut buf, 1)?;
    check_invariants(&buf)?;
    ensure!(
        slice(&buf, 505, 4)? == [0; 4],
        "dead cell bytes were not zeroed"
    );
    ensure!(
        slice(&buf, 40, 4)? == [0; 4],
        "stale last slot was not zeroed"
    );
    ensure!(slice(&buf, 32, 4)? == [0xFD, 0x01, 0x03, 0x00], "slot 0");
    ensure!(
        slice(&buf, 36, 4)? == [0xF7, 0x01, 0x02, 0x00],
        "slot 1 (was slot 2)"
    );
    ensure!(fragmented_of(&buf)? == 4, "fragmented_bytes");
    ensure!(free_end_of(&buf)? == 503, "free_end");
    ensure!(
        cells_of(&buf)? == vec![b"aaa".to_vec(), b"cc".to_vec()],
        "surviving cells"
    );
    Ok(())
}

#[test]
fn remove_last_cell_leaves_empty_fragmented_page() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    insert(&mut buf, 0, b"hello")?;
    remove(&mut buf, 0)?;
    check_invariants(&buf)?;
    ensure!(count_of(&buf)? == 0, "cell_count");
    ensure!(free_end_of(&buf)? == 507, "free_end must not move");
    ensure!(fragmented_of(&buf)? == 5, "fragmented_bytes");
    ensure!(slice(&buf, 507, 5)? == [0; 5], "dead bytes not zeroed");
    ensure!(slice(&buf, 32, 4)? == [0; 4], "stale slot not zeroed");

    for size in SIZES {
        // The largest cell, so fragmented_bytes must fit in a u16 even at 64 KiB.
        let max = sub(size, 36)?;
        let mut buf = empty_page(size)?;
        insert(&mut buf, 0, &vec![9; max])?;
        remove(&mut buf, 0)?;
        check_invariants(&buf)?;
        let page = SlottedPage::new(&buf)?;
        ensure!(page.cell_count() == 0, "size {size}: cell_count");
        ensure!(page.free_end() == 36, "size {size}: free_end");
        ensure!(page.fragmented_bytes() == max, "size {size}: fragmented");
        ensure!(
            page.fragmented_bytes() == sub(size, page.free_end())?,
            "size {size}: fragmented != size - free_end"
        );
        ensure!(
            page.total_free() == sub(size, 32)?,
            "size {size}: total_free"
        );
        page.validate()?;
    }
    Ok(())
}

#[test]
fn remove_out_of_range_is_rejected() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    let before = buf.clone();
    let error = remove(&mut buf, 0).err();
    ensure!(
        error
            == Some(PageError::SlotOutOfRange {
                index: 0,
                cell_count: 0
            }),
        "empty page: {error:?}"
    );
    ensure!(buf == before, "empty page changed");

    let mut buf = two_cell_page()?;
    let before = buf.clone();
    let error = remove(&mut buf, 2).err();
    ensure!(
        error
            == Some(PageError::SlotOutOfRange {
                index: 2,
                cell_count: 2
            }),
        "index == count: {error:?}"
    );
    ensure!(buf == before, "two-cell page changed");
    Ok(())
}

// ---------------------------------------------------------------------------
// Compaction (step 9)
// ---------------------------------------------------------------------------

#[test]
fn compact_rewrites_cells_in_logical_order() -> anyhow::Result<()> {
    // Before: hello (cell 0) at 507..512, ab (cell 1) at 105..107.
    // After: the 7 live bytes fill 505..512, cell 0 first: hello at 505..510,
    // ab at 510..512. hello moves down even though it was already at the end.
    let mut buf = fragmented_page()?;
    compact(&mut buf)?;
    check_compacted(&buf)?;
    ensure!(slice(&buf, 505, 7)? == *b"helloab", "cell area");
    // slot 0 = [505 = 0x01F9, 5], slot 1 = [510 = 0x01FE, 2]
    ensure!(slice(&buf, 32, 4)? == [0xF9, 0x01, 0x05, 0x00], "slot 0");
    ensure!(slice(&buf, 36, 4)? == [0xFE, 0x01, 0x02, 0x00], "slot 1");
    ensure!(slice(&buf, 16, 2)? == [0x02, 0x00], "cell_count");
    ensure!(slice(&buf, 18, 2)? == [0x00, 0x00], "fragmented_bytes");
    ensure!(slice(&buf, 20, 4)? == [0xF9, 0x01, 0x00, 0x00], "free_end");
    // Includes the old copy of ab at 105..107.
    ensure!(
        slice(&buf, 40, 465)?.iter().all(|&b| b == 0),
        "[40, 505) not zeroed"
    );
    let page = SlottedPage::new(&buf)?;
    ensure!(page.cell(0)? == b"hello", "cell(0)");
    ensure!(page.cell(1)? == b"ab", "cell(1)");
    ensure!(page.contiguous_free() == 465, "contiguous_free");
    ensure!(page.total_free() == 465, "total_free");
    page.validate()?;
    Ok(())
}

#[test]
fn compact_places_cell_zero_at_the_lowest_address() -> anyhow::Result<()> {
    // Appends store each new cell below the previous one, so the physical
    // order is the reverse of the logical order: aaa at 509, bbbb at 505,
    // cc at 503, d at 502.
    let mut buf = empty_page(512)?;
    insert(&mut buf, 0, b"aaa")?;
    insert(&mut buf, 1, b"bbbb")?;
    insert(&mut buf, 2, b"cc")?;
    insert(&mut buf, 3, b"d")?;
    remove(&mut buf, 1)?; // bbbb: hole at 505..509
    compact(&mut buf)?;
    check_compacted(&buf)?;
    // 6 live bytes fill 506..512 in logical order.
    ensure!(slice(&buf, 506, 6)? == *b"aaaccd", "cell area");
    // slot 0 = [506 = 0x01FA, 3], slot 1 = [509 = 0x01FD, 2],
    // slot 2 = [511 = 0x01FF, 1]
    ensure!(slice(&buf, 32, 4)? == [0xFA, 0x01, 0x03, 0x00], "slot 0");
    ensure!(slice(&buf, 36, 4)? == [0xFD, 0x01, 0x02, 0x00], "slot 1");
    ensure!(slice(&buf, 40, 4)? == [0xFF, 0x01, 0x01, 0x00], "slot 2");
    ensure!(slice(&buf, 20, 4)? == [0xFA, 0x01, 0x00, 0x00], "free_end");
    ensure!(
        slice(&buf, 44, 462)?.iter().all(|&b| b == 0),
        "[44, 506) not zeroed"
    );
    ensure!(
        cells_of(&buf)? == vec![b"aaa".to_vec(), b"cc".to_vec(), b"d".to_vec()],
        "logical order changed"
    );
    Ok(())
}

#[test]
fn compact_orders_cells_even_without_fragmented_bytes() -> anyhow::Result<()> {
    // Already in the compacted layout: nothing changes.
    for buf in [empty_page(512)?, two_cell_page()?] {
        let mut copy = buf.clone();
        compact(&mut copy)?;
        ensure!(copy == buf, "compact changed an already compacted page");
    }
    // hello (cell 0) at 507, ab (cell 1) at 505: no holes, but the physical
    // order is reversed, so compact rewrites it.
    let mut buf = empty_page(512)?;
    insert(&mut buf, 0, b"hello")?;
    insert(&mut buf, 1, b"ab")?;
    ensure!(fragmented_of(&buf)? == 0, "fixture has fragmented bytes");
    compact(&mut buf)?;
    check_compacted(&buf)?;
    ensure!(slice(&buf, 505, 7)? == *b"helloab", "cell area");
    Ok(())
}

#[test]
fn compact_gives_the_same_bytes_for_the_same_cells() -> anyhow::Result<()> {
    // Two pages with the cells hello, ab built by different histories: one
    // with a 400-byte hole, one by plain appends. After compact they are
    // byte-for-byte equal.
    let mut with_hole = fragmented_page()?;
    let mut appended = empty_page(512)?;
    insert(&mut appended, 0, b"hello")?;
    insert(&mut appended, 1, b"ab")?;
    compact(&mut with_hole)?;
    compact(&mut appended)?;
    ensure!(with_hole == appended, "same cells, different bytes");
    Ok(())
}

#[test]
fn compact_twice_equals_compact_once() -> anyhow::Result<()> {
    let mut buf = fragmented_page()?;
    compact(&mut buf)?;
    let once = buf.clone();
    compact(&mut buf)?;
    ensure!(buf == once, "a second compact changed the page");
    Ok(())
}

#[test]
fn compact_page_with_only_dead_cells_resets_free_end() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    insert(&mut buf, 0, b"hello")?;
    remove(&mut buf, 0)?;
    compact(&mut buf)?;
    check_compacted(&buf)?;
    ensure!(
        slice(&buf, 16, 4)? == [0, 0, 0, 0],
        "cell_count or fragmented"
    );
    ensure!(slice(&buf, 20, 4)? == [0x00, 0x02, 0x00, 0x00], "free_end");

    for size in SIZES {
        // The largest cell, so free_end goes from 36 back to the page size,
        // which needs the full u32 at 64 KiB.
        let mut buf = empty_page(size)?;
        insert(&mut buf, 0, &vec![9; sub(size, 36)?])?;
        remove(&mut buf, 0)?;
        compact(&mut buf)?;
        check_compacted(&buf)?;
        ensure!(free_end_of(&buf)? == size, "size {size}: free_end");
        ensure!(
            slice(&buf, 32, sub(size, 32)?)?.iter().all(|&b| b == 0),
            "size {size}: body not zero"
        );
        let page = SlottedPage::new(&buf)?;
        ensure!(page.cell_count() == 0, "size {size}: cell_count");
        ensure!(
            page.contiguous_free() == sub(size, 32)?,
            "size {size}: contiguous_free"
        );
    }
    let buf = {
        let mut buf = empty_page(65536)?;
        insert(&mut buf, 0, &vec![9; 65500])?;
        remove(&mut buf, 0)?;
        compact(&mut buf)?;
        buf
    };
    ensure!(
        slice(&buf, 20, 4)? == [0x00, 0x00, 0x01, 0x00],
        "64 KiB free_end bytes"
    );
    Ok(())
}

#[test]
fn compact_does_not_touch_checksum_common_header_or_flags() -> anyhow::Result<()> {
    let mut buf = fragmented_page()?;
    put(&mut buf, 0, &[1, 2, 3, 4])?;
    put(&mut buf, 5, &[0x11; 11])?;
    put(
        &mut buf,
        24,
        &[0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87],
    )?;
    let before = protected_bytes(&buf)?;
    compact(&mut buf)?;
    ensure!(protected_bytes(&buf)? == before, "protected bytes changed");
    ensure!(buf.get(4) == Some(&3), "type byte lost");
    check_compacted(&buf)
}

#[test]
fn insert_compacts_when_only_fragmented_space_is_free() -> anyhow::Result<()> {
    // Before step 9 this insert failed with PageFull { needed: 5,
    // contiguous: 4, total_free: 480 }.
    let mut buf = empty_page(512)?;
    insert(&mut buf, 0, &[0x33; 476])?;
    remove(&mut buf, 0)?;
    insert(&mut buf, 0, &[0x44])?;
    check_no_holes(&buf)?;
    ensure!(buf.get(511) == Some(&0x44), "cell byte at 511");
    // slot 0 = [offset 511 = 0x01FF, len 1]
    ensure!(slice(&buf, 32, 4)? == [0xFF, 0x01, 0x01, 0x00], "slot 0");
    ensure!(slice(&buf, 20, 4)? == [0xFF, 0x01, 0x00, 0x00], "free_end");
    ensure!(
        slice(&buf, 36, 475)?.iter().all(|&b| b == 0),
        "[36, 511) not zeroed"
    );
    Ok(())
}

#[test]
fn insert_that_fits_contiguous_space_does_not_compact() -> anyhow::Result<()> {
    // contiguous_free is 65, so a 61-byte cell (65 with its slot) fits exactly.
    let mut buf = fragmented_page()?;
    insert(&mut buf, 1, &[0x77; 61])?;
    check_invariants(&buf)?;
    check_free_gap_zeroed(&buf)?;
    ensure!(fragmented_of(&buf)? == 400, "fragmented_bytes changed");
    ensure!(slice(&buf, 32, 4)? == [0xFB, 0x01, 0x05, 0x00], "slot 0");
    // slot 1 = [44, 61], slot 2 = [105, 2]: ab did not move.
    ensure!(slice(&buf, 36, 4)? == [0x2C, 0x00, 0x3D, 0x00], "slot 1");
    ensure!(slice(&buf, 40, 4)? == [0x69, 0x00, 0x02, 0x00], "slot 2");
    ensure!(slice(&buf, 105, 2)? == *b"ab", "ab moved");
    ensure!(free_end_of(&buf)? == 44, "free_end");
    let page = SlottedPage::new(&buf)?;
    ensure!(page.contiguous_free() == 0, "contiguous_free");
    ensure!(page.total_free() == 400, "total_free");
    Ok(())
}

#[test]
fn insert_one_byte_past_contiguous_space_compacts() -> anyhow::Result<()> {
    // A 62-byte cell needs 66 > 65 contiguous bytes, so insert compacts first:
    // hello goes to 505..510 and ab to 510..512. Then the new cell goes to
    // free_end like any insert (443..505), even though it is logical cell 1.
    let mut buf = fragmented_page()?;
    insert(&mut buf, 1, &[0x77; 62])?;
    check_no_holes(&buf)?;
    ensure!(slice(&buf, 32, 4)? == [0xF9, 0x01, 0x05, 0x00], "slot 0");
    // slot 1 = [443 = 0x01BB, 62]
    ensure!(slice(&buf, 36, 4)? == [0xBB, 0x01, 0x3E, 0x00], "slot 1");
    // slot 2 = [510 = 0x01FE, 2]
    ensure!(slice(&buf, 40, 4)? == [0xFE, 0x01, 0x02, 0x00], "slot 2");
    ensure!(slice(&buf, 20, 4)? == [0xBB, 0x01, 0x00, 0x00], "free_end");
    let expected = vec![b"hello".to_vec(), vec![0x77; 62], b"ab".to_vec()];
    ensure!(cells_of(&buf)? == expected, "cells");
    Ok(())
}

#[test]
fn insert_uses_all_free_space_and_then_fails() -> anyhow::Result<()> {
    // total_free is 465: a 461-byte cell (465 with its slot) fits exactly.
    let mut buf = fragmented_page()?;
    insert(&mut buf, 2, &[0x55; 461])?;
    check_no_holes(&buf)?;
    // slot 2 = [44, 461 = 0x01CD]
    ensure!(slice(&buf, 40, 4)? == [0x2C, 0x00, 0xCD, 0x01], "slot 2");
    let page = SlottedPage::new(&buf)?;
    ensure!(
        page.free_start() == 44 && page.free_end() == 44,
        "free_start/free_end"
    );
    ensure!(page.total_free() == 0, "total_free");

    // One byte more does not fit even after compaction. The page is not
    // compacted on the way to the error.
    let mut buf = fragmented_page()?;
    let before = buf.clone();
    let error = insert(&mut buf, 2, &[0x55; 462]).err();
    ensure!(
        error
            == Some(PageError::PageFull {
                needed: 466,
                contiguous: 65,
                total_free: 465
            }),
        "got {error:?}"
    );
    ensure!(buf == before, "the failed insert changed the buffer");
    Ok(())
}

#[test]
fn compact_on_corrupt_page_fails_without_writing() -> anyhow::Result<()> {
    let valid = fragmented_page()?;
    let mut cases: Vec<(String, Vec<u8>)> = Vec::new();
    // Slot 1 points into hello. Every slot is in range and the sum matches
    // (5 + 2 + 400 = 407), so only the overlap check can catch it.
    let mut buf = valid.clone();
    put_slot(&mut buf, 1, 507, 2)?;
    cases.push(("overlapping cells".into(), buf));
    // fragmented_bytes off by one: the header check passes (it is below the
    // cell area of 407), but the sum does not match.
    for fragmented in [399u16, 401] {
        let mut buf = valid.clone();
        put_u16(&mut buf, OFF_FRAGMENTED, fragmented)?;
        cases.push((format!("fragmented {fragmented}"), buf));
    }

    for (what, buf) in &cases {
        mut_view_rejects(buf, &format!("{what}: compact"), compact_view)?;
        // 104 bytes needed > 65 contiguous, so this insert must compact.
        mut_view_rejects(buf, &format!("{what}: insert that compacts"), |p| {
            p.insert(0, &[1; 100])
        })?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[test]
fn search_by_on_empty_page_returns_err_0() -> anyhow::Result<()> {
    let buf = empty_page(512)?;
    let page = SlottedPage::new(&buf)?;
    let mut calls = 0usize;
    let result = page.search_by(|_| {
        calls = calls.saturating_add(1);
        Ordering::Equal
    })?;
    ensure!(result == Err(0), "got {result:?}");
    ensure!(
        calls == 0,
        "the comparator ran {calls} times on an empty page"
    );
    Ok(())
}

#[test]
fn search_by_finds_existing_and_insert_positions() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    for (i, key) in [b"b", b"d", b"f"].into_iter().enumerate() {
        insert(&mut buf, i, key)?;
    }
    let page = SlottedPage::new(&buf)?;
    let search = |key: &[u8]| page.search_by(|cell| cell.cmp(key));
    for (key, expected) in [
        (b"b", Ok(0)),
        (b"d", Ok(1)),
        (b"f", Ok(2)),
        (b"a", Err(0)),
        (b"c", Err(1)),
        (b"e", Err(2)),
        (b"g", Err(3)),
    ] {
        let got = search(key)?;
        ensure!(
            got == expected,
            "key {key:?}: got {got:?}, expected {expected:?}"
        );
    }
    Ok(())
}

#[test]
fn search_by_drives_sorted_inserts() -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    for key in [30u8, 10, 20, 40, 0, 25] {
        let needle = [key];
        let position = {
            let page = SlottedPage::new(&buf)?;
            match page.search_by(|cell| cell.cmp(needle.as_slice()))? {
                Ok(position) | Err(position) => position,
            }
        };
        insert(&mut buf, position, &needle)?;
        check_invariants(&buf)?;
    }
    let expected: Vec<Vec<u8>> = [0u8, 10, 20, 25, 30, 40].iter().map(|&k| vec![k]).collect();
    ensure!(
        cells_of(&buf)? == expected,
        "cells are {:?}",
        cells_of(&buf)?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Corruption
// ---------------------------------------------------------------------------

/// Header fields that `new` must reject on its own (O(1) checks), through both
/// views, with no write.
fn header_corruption_is_rejected(buf: &[u8], what: &str) -> anyhow::Result<()> {
    expect_corrupt(SlottedPage::new(buf), &format!("{what}: SlottedPage::new"))?;
    let mut copy = buf.to_vec();
    expect_corrupt(
        SlottedPageMut::new(&mut copy),
        &format!("{what}: SlottedPageMut::new"),
    )?;
    ensure!(
        copy == buf,
        "{what}: SlottedPageMut::new wrote to the buffer"
    );
    mut_view_rejects(buf, &format!("{what}: insert"), |p| p.insert(0, b"x"))?;
    mut_view_rejects(buf, &format!("{what}: remove"), |p| p.remove(0))?;
    mut_view_rejects(buf, &format!("{what}: compact"), compact_view)?;
    Ok(())
}

#[test]
fn free_end_past_page_size_is_corrupt() -> anyhow::Result<()> {
    for value in [513u32, 65536, u32::MAX] {
        let mut buf = two_cell_page()?;
        put_u32(&mut buf, OFF_FREE_END, value)?;
        header_corruption_is_rejected(&buf, &format!("free_end {value}"))?;
    }
    Ok(())
}

#[test]
fn free_start_past_free_end_is_corrupt() -> anyhow::Result<()> {
    // free_end is 505, so 119 slots end at 32 + 476 = 508.
    for count in [119u16, 0xFFFF] {
        let mut buf = two_cell_page()?;
        put_u16(&mut buf, OFF_CELL_COUNT, count)?;
        header_corruption_is_rejected(&buf, &format!("cell_count {count}"))?;
    }
    // Two slots end at 40, so a free_end below 40 leaves no room for them.
    for free_end in [0u32, 39] {
        let mut buf = two_cell_page()?;
        put_u32(&mut buf, OFF_FREE_END, free_end)?;
        header_corruption_is_rejected(&buf, &format!("free_end {free_end}"))?;
    }
    Ok(())
}

#[test]
fn fragmented_larger_than_cell_area_is_corrupt() -> anyhow::Result<()> {
    // The cell area is 512 - 505 = 7 bytes.
    for value in [8u16, 0xFFFF] {
        let mut buf = two_cell_page()?;
        put_u16(&mut buf, OFF_FRAGMENTED, value)?;
        header_corruption_is_rejected(&buf, &format!("fragmented {value}"))?;
    }
    Ok(())
}

/// Runs the checks that every corrupt slot 0 must fail: `validate`, reading the
/// cell, and removing it, all without writes.
fn bad_slot_zero_is_rejected(buf: &[u8], what: &str) -> anyhow::Result<()> {
    validate_is_corrupt(buf, &format!("{what}: validate"))?;
    read_view_rejects(buf, &format!("{what}: cell(0)"), |p| p.cell(0).map(|_| ()))?;
    mut_view_rejects(buf, &format!("{what}: validate (mut)"), |p| {
        p.view().validate()
    })?;
    mut_view_rejects(buf, &format!("{what}: remove(0)"), |p| p.remove(0))?;
    // compact validates the whole page first, even when fragmented_bytes is 0.
    mut_view_rejects(buf, &format!("{what}: compact"), compact_view)?;

    // insert may not look at the slots, so it is allowed to succeed on a page
    // with a bad slot, but it must not panic and a failure must not write.
    let mut copy = buf.to_vec();
    let outcome = SlottedPageMut::new(&mut copy).and_then(|mut p| p.insert(0, b"x"));
    if outcome.is_err() {
        ensure!(copy == buf, "{what}: a failed insert wrote to the buffer");
    }
    Ok(())
}

#[test]
fn slot_offset_before_free_end_is_corrupt() -> anyhow::Result<()> {
    // free_end is 505.
    for offset in [504u16, 32, 0] {
        let mut buf = two_cell_page()?;
        put_slot(&mut buf, 0, offset, 2)?;
        bad_slot_zero_is_rejected(&buf, &format!("slot offset {offset}"))?;
    }
    Ok(())
}

#[test]
fn slot_past_page_end_is_corrupt() -> anyhow::Result<()> {
    for (offset, len) in [
        (511u16, 2u16),
        (510, 3),
        (507, 6),
        (0xFFFF, 0xFFFF),
        (505, 0xFFFF),
    ] {
        let mut buf = two_cell_page()?;
        put_slot(&mut buf, 0, offset, len)?;
        bad_slot_zero_is_rejected(&buf, &format!("slot [{offset}, {len}]"))?;
    }
    Ok(())
}

#[test]
fn slot_with_zero_len_is_corrupt() -> anyhow::Result<()> {
    let mut buf = two_cell_page()?;
    put_slot(&mut buf, 0, 505, 0)?;
    bad_slot_zero_is_rejected(&buf, "slot with len 0")
}

#[test]
fn validate_detects_sum_mismatch() -> anyhow::Result<()> {
    // Live cells take 7 bytes and fragmented_bytes is 0, so the cell area is 7.
    let valid = two_cell_page()?;
    let mut buf = valid.clone();
    // Header-level checks pass (1 <= 7), but 7 != 7 + 1.
    put_u16(&mut buf, OFF_FRAGMENTED, 1)?;
    validate_is_corrupt(&buf, "fragmented too big by 1")?;

    // Sum of live lengths too small: 1 + 5 = 6, but the cell area is 7.
    let mut buf = valid.clone();
    put_slot(&mut buf, 0, 505, 1)?;
    validate_is_corrupt(&buf, "live sum too small")?;

    // Sum of live lengths too big: 3 + 5 = 8, but the cell area is 7.
    let mut buf = valid;
    put_slot(&mut buf, 0, 505, 3)?;
    validate_is_corrupt(&buf, "live sum too big")?;
    Ok(())
}

#[test]
fn mutations_on_corrupt_page_fail_without_writing() -> anyhow::Result<()> {
    let mut cases: Vec<(String, Vec<u8>)> = Vec::new();
    let mut buf = two_cell_page()?;
    put_u32(&mut buf, OFF_FREE_END, 513)?;
    cases.push(("free_end past the page".into(), buf));
    let mut buf = two_cell_page()?;
    put_u16(&mut buf, OFF_CELL_COUNT, 0xFFFF)?;
    cases.push(("cell_count 0xFFFF".into(), buf));
    let mut buf = two_cell_page()?;
    put_u16(&mut buf, OFF_FRAGMENTED, 0xFFFF)?;
    cases.push(("fragmented 0xFFFF".into(), buf));
    let mut buf = two_cell_page()?;
    put_slot(&mut buf, 0, 510, 5)?;
    cases.push(("slot past the page end".into(), buf));
    let mut buf = two_cell_page()?;
    put_slot(&mut buf, 0, 505, 0)?;
    cases.push(("slot with len 0".into(), buf));

    for (what, buf) in &cases {
        mut_view_rejects(buf, &format!("{what}: remove(0)"), |p| p.remove(0))?;
        mut_view_rejects(buf, &format!("{what}: compact"), compact_view)?;
    }
    // insert at the end must also fail without writing when the header is bad.
    for (what, buf) in cases.iter().take(3) {
        mut_view_rejects(buf, &format!("{what}: insert"), |p| p.insert(0, b"x"))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

/// Calls every public operation on `original`. Results are ignored: the point
/// is that nothing panics. If `validate` accepts the page, every cell must be
/// readable.
fn exercise(original: &[u8]) -> Result<(), TestCaseError> {
    if let Ok(page) = SlottedPage::new(original) {
        let count = page.cell_count();
        let _ = (
            page.fragmented_bytes(),
            page.free_start(),
            page.free_end(),
            page.contiguous_free(),
            page.total_free(),
        );
        for i in 0..count.min(16) {
            let _ = page.cell(i);
        }
        let _ = page.cell(count);
        let _ = page.search_by(|_| Ordering::Less);
        let _ = page.search_by(|cell| cell.len().cmp(&3));
        if page.validate().is_ok() {
            for i in 0..count {
                prop_assert!(
                    page.cell(i).is_ok(),
                    "validate passed but cell({}) failed",
                    i
                );
            }
            prop_assert!(
                matches!(page.cell(count), Err(PageError::SlotOutOfRange { .. })),
                "cell(count) did not fail with SlotOutOfRange"
            );
        }
    }
    let mut copy = original.to_vec();
    if let Ok(mut page) = SlottedPageMut::new(&mut copy) {
        let _ = page.insert(0, b"xyz");
        let _ = page.remove(0);
        let count = page.view().cell_count();
        let _ = page.insert(count, b"q");
        let _ = page.remove(count);
        let _ = page.view().validate();
        // compact validates first, and compaction keeps a valid page valid.
        if page.compact().is_ok() {
            prop_assert!(
                page.view().validate().is_ok(),
                "compact succeeded but left an invalid page"
            );
        }
    }
    Ok(())
}

/// One random operation for `run_ops_against_model`.
#[derive(Debug, Clone)]
enum Op {
    Insert(Index, Vec<u8>),
    Remove(Index),
    Compact,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (any::<Index>(), prop::collection::vec(any::<u8>(), 1..=120))
            .prop_map(|(index, cell)| Op::Insert(index, cell)),
        2 => any::<Index>().prop_map(Op::Remove),
        1 => Just(Op::Compact),
    ]
}

/// Total free bytes of a 512-byte page holding `model`, computed from the
/// model: 480 bytes of room minus one slot and the bytes of each cell.
fn model_total_free(model: &[Vec<u8>]) -> anyhow::Result<usize> {
    let live = model
        .iter()
        .try_fold(0usize, |acc, cell| acc.checked_add(cell.len()))
        .context("live bytes overflow")?;
    sub(480, add(mul(4, model.len())?, live)?)
}

/// Applies `ops` to a 512-byte page and to a `Vec` model. An insert must
/// succeed exactly when the model says the cell fits in the total free space,
/// and it must compact exactly when the cell does not fit in the contiguous
/// space. A failed insert is `PageFull` and writes nothing.
fn run_ops_against_model(ops: Vec<Op>) -> anyhow::Result<()> {
    let mut buf = empty_page(512)?;
    let mut model: Vec<Vec<u8>> = Vec::new();
    for op in ops {
        match op {
            Op::Insert(selector, cell) => {
                let index = selector.index(add(model.len(), 1)?);
                let needed = add(cell.len(), 4)?;
                let total_free = model_total_free(&model)?;
                let contiguous = sub(free_end_of(&buf)?, free_start_of(&buf)?)?;
                let fragmented = fragmented_of(&buf)?;
                if needed <= total_free {
                    insert(&mut buf, index, &cell)
                        .with_context(|| format!("insert {needed} bytes at {index}"))?;
                    model.insert(index, cell);
                    let after = fragmented_of(&buf)?;
                    if needed <= contiguous {
                        ensure!(after == fragmented, "insert compacted without need");
                    } else {
                        ensure!(after == 0, "insert did not compact");
                    }
                } else {
                    let before = buf.clone();
                    let error = insert(&mut buf, index, &cell).err();
                    ensure!(
                        error
                            == Some(PageError::PageFull {
                                needed,
                                contiguous,
                                total_free
                            }),
                        "insert {needed} bytes: got {error:?}"
                    );
                    ensure!(buf == before, "a failed insert changed the buffer");
                }
            }
            Op::Remove(selector) => {
                if model.is_empty() {
                    continue;
                }
                let index = selector.index(model.len());
                remove(&mut buf, index).with_context(|| format!("remove {index}"))?;
                model.remove(index);
            }
            Op::Compact => {
                compact(&mut buf).context("compact")?;
                check_compacted(&buf)?;
            }
        }
        check_invariants(&buf)?;
        check_free_gap_zeroed(&buf)?;
        ensure!(cells_of(&buf)? == model, "cells differ from the model");
        let page = SlottedPage::new(&buf)?;
        ensure!(
            page.total_free() == model_total_free(&model)?,
            "total_free {} differs from the model",
            page.total_free()
        );
    }
    Ok(())
}

proptest! {
    #[test]
    fn prop_random_bytes_never_panic(
        bytes in prop::collection::vec(any::<u8>(), 512)
    ) {
        let mut buf = bytes;
        if let Some(type_byte) = buf.get_mut(4) {
            *type_byte = 3;
        }
        exercise(&buf)?;
    }

    #[test]
    fn prop_mutated_valid_page_never_panics(
        edits in prop::collection::vec((16usize..64, any::<u8>()), 1..5)
    ) {
        let mut buf = two_cell_page().map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
        for (at, value) in edits {
            if let Some(byte) = buf.get_mut(at) {
                *byte = value;
            }
        }
        exercise(&buf)?;
    }

    #[test]
    fn prop_insert_then_read_round_trips(
        ops in prop::collection::vec(
            (any::<Index>(), prop::collection::vec(any::<u8>(), 1..=60)),
            1..=60
        )
    ) {
        // 60 cells of at most 60 bytes need at most 60 * 64 = 3840 bytes, which
        // fits in a 4096-byte page (4064 bytes of room), so no insert fails.
        let fail = |e: anyhow::Error| TestCaseError::fail(format!("{e:#}"));
        let mut buf = empty_page(4096).map_err(fail)?;
        let mut model: Vec<Vec<u8>> = Vec::new();
        for (selector, cell) in ops {
            let index = selector.index(model.len().saturating_add(1));
            insert(&mut buf, index, &cell)
                .map_err(|e| TestCaseError::fail(format!("insert at {index}: {e}")))?;
            model.insert(index, cell);
            check_invariants(&buf).map_err(fail)?;
            let cells = cells_of(&buf).map_err(fail)?;
            prop_assert_eq!(&cells, &model);
        }
        let page = SlottedPage::new(&buf)
            .map_err(|e| TestCaseError::fail(e.to_string()))?;
        prop_assert!(page.validate().is_ok());
    }

    #[test]
    fn prop_insert_remove_compact_match_model(
        ops in prop::collection::vec(op_strategy(), 1..=80)
    ) {
        run_ops_against_model(ops).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}

// ---------------------------------------------------------------------------
// Model tests (step 10)
// ---------------------------------------------------------------------------

/// One random operation for the sorted-leaf model. Keys are `u16` in `0..64`.
#[derive(Debug, Clone)]
enum LeafOp {
    Put(u16, Vec<u8>),
    Delete(u16),
    Get(u16),
    Compact,
}

/// The cell a B+Tree leaf stores for `key` and `value`: the key in big-endian
/// (so the byte order is the numeric order), then the value.
fn leaf_cell(key: u16, value: &[u8]) -> Vec<u8> {
    let mut cell = key.to_be_bytes().to_vec();
    cell.extend_from_slice(value);
    cell
}

/// Compares the key stored at the start of `cell` with `key`.
fn cmp_leaf_key(cell: &[u8], key: u16) -> Ordering {
    let wanted = key.to_be_bytes();
    cell.get(..2).unwrap_or_default().cmp(wanted.as_slice())
}

/// Number of keys in `model` below `key`: the rank the page must report.
fn leaf_rank(model: &BTreeMap<u16, Vec<u8>>, key: u16) -> usize {
    model.range(..key).count()
}

/// Total free bytes of a leaf page of `size` bytes holding `model`: the page
/// minus the 32-byte header, minus for each entry one 4-byte slot, the 2-byte
/// key and the value (ADR 0003).
fn leaf_model_free(size: usize, model: &BTreeMap<u16, Vec<u8>>) -> anyhow::Result<usize> {
    let used = model.values().try_fold(0usize, |acc, value| {
        let entry = add(add(SLOT_SIZE, 2)?, value.len())?;
        add(acc, entry)
    })?;
    sub(sub(size, SLOTTED_HEADER_SIZE)?, used)
}

fn leaf_op_strategy(size: usize) -> impl Strategy<Value = LeafOp> {
    // The largest value that fits in an empty page: max_cell_len minus the key.
    let max_value = size
        .saturating_sub(SLOTTED_HEADER_SIZE)
        .saturating_sub(SLOT_SIZE)
        .saturating_sub(2);
    let value = prop_oneof![
        8 => prop::collection::vec(any::<u8>(), 0..=32usize),
        1 => prop::collection::vec(any::<u8>(), 0..=max_value),
    ];
    prop_oneof![
        4 => (0u16..64, value).prop_map(|(key, value)| LeafOp::Put(key, value)),
        2 => (0u16..64).prop_map(LeafOp::Delete),
        2 => (0u16..64).prop_map(LeafOp::Get),
        1 => Just(LeafOp::Compact),
    ]
}

/// Searches the page for `key` with the leaf comparator.
fn leaf_search(buf: &[u8], key: u16) -> anyhow::Result<Result<usize, usize>> {
    let page = SlottedPage::new(buf)?;
    Ok(page.search_by(|cell| cmp_leaf_key(cell, key))?)
}

/// Runs `ops` on a page of `size` bytes used as a sorted B+Tree leaf and on a
/// `BTreeMap` model. Operations that cannot happen (a put that does not fit,
/// a delete of a missing key) are no-ops that check the page refuses or
/// ignores them, so shrinking stays valid.
fn run_leaf_ops(size: usize, ops: Vec<LeafOp>) -> anyhow::Result<()> {
    let mut buf = empty_page(size)?;
    let mut model: BTreeMap<u16, Vec<u8>> = BTreeMap::new();
    for op in ops {
        match op {
            LeafOp::Put(key, value) => {
                let cell = leaf_cell(key, &value);
                let found = leaf_search(&buf, key)?;
                let rank = leaf_rank(&model, key);
                if model.contains_key(&key) {
                    ensure!(found == Ok(rank), "put {key}: present, got {found:?}");
                } else {
                    ensure!(found == Err(rank), "put {key}: absent, got {found:?}");
                    let needed = add(cell.len(), SLOT_SIZE)?;
                    if needed <= leaf_model_free(size, &model)? {
                        insert(&mut buf, rank, &cell)
                            .with_context(|| format!("put {key} at rank {rank}"))?;
                        model.insert(key, value);
                    } else {
                        let before = buf.clone();
                        let error = insert(&mut buf, rank, &cell).err();
                        ensure!(
                            matches!(error, Some(PageError::PageFull { .. })),
                            "put {key}: expected PageFull, got {error:?}"
                        );
                        ensure!(buf == before, "a failed put changed the buffer");
                    }
                }
            }
            LeafOp::Delete(key) => {
                let found = leaf_search(&buf, key)?;
                let rank = leaf_rank(&model, key);
                if model.remove(&key).is_some() {
                    ensure!(found == Ok(rank), "delete {key}: present, got {found:?}");
                    remove(&mut buf, rank).with_context(|| format!("delete rank {rank}"))?;
                } else {
                    ensure!(found == Err(rank), "delete {key}: absent, got {found:?}");
                }
            }
            LeafOp::Get(key) => {
                let found = leaf_search(&buf, key)?;
                let rank = leaf_rank(&model, key);
                match model.get(&key) {
                    Some(value) => {
                        ensure!(found == Ok(rank), "get {key}: present, got {found:?}");
                        let page = SlottedPage::new(&buf)?;
                        ensure!(
                            page.cell(rank)? == leaf_cell(key, value).as_slice(),
                            "get {key}: cell bytes differ"
                        );
                    }
                    None => ensure!(found == Err(rank), "get {key}: absent, got {found:?}"),
                }
            }
            LeafOp::Compact => {
                compact(&mut buf).context("compact")?;
                check_compacted(&buf)?;
            }
        }
        check_invariants(&buf)?;
        check_free_gap_zeroed(&buf)?;
        let expected: Vec<Vec<u8>> = model.iter().map(|(k, v)| leaf_cell(*k, v)).collect();
        ensure!(cells_of(&buf)? == expected, "cells differ from the model");
        let page = SlottedPage::new(&buf)?;
        ensure!(
            page.total_free() == leaf_model_free(size, &model)?,
            "total_free {} differs from the model",
            page.total_free()
        );
        ensure!(page.validate().is_ok(), "validate failed");
    }
    Ok(())
}

/// Builds a 512-byte page from `ops` (inserts that do not fit are ignored),
/// compacts it, and returns it with the cells it must hold.
fn build_compacted(ops: Vec<Op>) -> anyhow::Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let mut buf = empty_page(512)?;
    let mut model: Vec<Vec<u8>> = Vec::new();
    for op in ops {
        match op {
            Op::Insert(selector, cell) => {
                let index = selector.index(add(model.len(), 1)?);
                match insert(&mut buf, index, &cell) {
                    Ok(()) => model.insert(index, cell),
                    Err(PageError::PageFull { .. }) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Op::Remove(selector) => {
                if !model.is_empty() {
                    let index = selector.index(model.len());
                    remove(&mut buf, index)?;
                    model.remove(index);
                }
            }
            Op::Compact => compact(&mut buf)?,
        }
    }
    compact(&mut buf)?;
    Ok((buf, model))
}

/// Compacting a page gives the same bytes as appending the same cells to an
/// empty page and compacting, whatever the history that produced the cells.
fn check_history_independence(ops: Vec<Op>) -> anyhow::Result<()> {
    let (a, model) = build_compacted(ops)?;
    let mut b = empty_page(512)?;
    for (index, cell) in model.iter().enumerate() {
        insert(&mut b, index, cell).with_context(|| format!("rebuild: insert {index}"))?;
    }
    compact(&mut b)?;
    ensure!(a == b, "same cells, different bytes after compact");
    let mut again = a.clone();
    compact(&mut again)?;
    ensure!(again == a, "a second compact changed the page");
    Ok(())
}

proptest! {
    #[test]
    fn prop_sorted_leaf_matches_btreemap(
        (size, ops) in prop::sample::select(vec![512usize, 4096, 65536]).prop_flat_map(
            |size| (Just(size), prop::collection::vec(leaf_op_strategy(size), 1..=100))
        )
    ) {
        run_leaf_ops(size, ops).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }

    #[test]
    fn prop_compaction_is_history_independent(
        ops in prop::collection::vec(op_strategy(), 1..=80)
    ) {
        check_history_independence(ops)
            .map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
