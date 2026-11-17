//! Tests for the slotted page (`storage::pager::slotted`, step 8).
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
//!   Fragmented space is not reused by `insert` yet (compaction is step 9).
//! - `search_by` behaves like `slice::binary_search_by`.
//! - Corrupted pages (bad header fields, bad slots, a sum mismatch) are always
//!   an `Err`, never a panic, and mutations on them write nothing.
//! - Every operation keeps the invariants I1 to I4 of ADR 0003. They are
//!   checked from the raw bytes by `check_invariants`, not through the API.
//!   Two property tests check random bytes (no panics) and random inserts
//!   against a `Vec` model.
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
//!
//! New PageError variants: EmptyCell, CellTooLarge { len, max },
//! SlotOutOfRange { index, cell_count }, PageFull { needed, contiguous,
//! total_free }, CorruptSlottedPage { .. }
//! ```

use std::cmp::Ordering;

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

fn add(a: usize, b: usize) -> anyhow::Result<usize> {
    a.checked_add(b).context("add overflow")
}

fn sub(a: usize, b: usize) -> anyhow::Result<usize> {
    a.checked_sub(b).context("sub underflow")
}

fn mul(a: usize, b: usize) -> anyhow::Result<usize> {
    a.checked_mul(b).context("mul overflow")
}

/// `len` bytes of `buf` starting at `at`.
fn slice(buf: &[u8], at: usize, len: usize) -> anyhow::Result<&[u8]> {
    let end = add(at, len)?;
    buf.get(at..end)
        .with_context(|| format!("range {at}..{end} past buffer of {}", buf.len()))
}

/// Overwrites bytes of `buf` starting at `at`.
fn put(buf: &mut [u8], at: usize, bytes: &[u8]) -> anyhow::Result<()> {
    let end = add(at, bytes.len())?;
    buf.get_mut(at..end)
        .context("range past buffer")?
        .copy_from_slice(bytes);
    Ok(())
}

fn u16_at(buf: &[u8], at: usize) -> anyhow::Result<u16> {
    Ok(u16::from_le_bytes(slice(buf, at, 2)?.try_into()?))
}

fn u32_at(buf: &[u8], at: usize) -> anyhow::Result<u32> {
    Ok(u32::from_le_bytes(slice(buf, at, 4)?.try_into()?))
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

fn count_of(buf: &[u8]) -> anyhow::Result<usize> {
    Ok(usize::from(u16_at(buf, OFF_CELL_COUNT)?))
}

fn fragmented_of(buf: &[u8]) -> anyhow::Result<usize> {
    Ok(usize::from(u16_at(buf, OFF_FRAGMENTED)?))
}

fn free_end_of(buf: &[u8]) -> anyhow::Result<usize> {
    Ok(usize::try_from(u32_at(buf, OFF_FREE_END)?)?)
}

/// A page of `size` bytes, prefilled with 0xAA and then initialized, so any
/// byte `init` forgets to write shows up.
fn empty_page(size: usize) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0xAA; size];
    SlottedPageMut::init(&mut buf)?;
    Ok(buf)
}

fn insert(buf: &mut [u8], index: usize, cell: &[u8]) -> Result<(), PageError> {
    SlottedPageMut::new(buf)?.insert(index, cell)
}

fn remove(buf: &mut [u8], index: usize) -> Result<(), PageError> {
    SlottedPageMut::new(buf)?.remove(index)
}

/// Every cell of the page, in logical order, copied out.
fn cells_of(buf: &[u8]) -> anyhow::Result<Vec<Vec<u8>>> {
    let page = SlottedPage::new(buf)?;
    (0..page.cell_count())
        .map(|i| Ok(page.cell(i)?.to_vec()))
        .collect()
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

/// Checks the invariants I1 to I4 of ADR 0003 from the raw bytes, without
/// using the API under test. I3 (no overlap) sorts the slots by offset.
fn check_invariants(buf: &[u8]) -> anyhow::Result<()> {
    let size = buf.len();
    ensure!(buf.get(4) == Some(&3), "page type byte is {:?}", buf.get(4));
    let count = count_of(buf)?;
    let fragmented = fragmented_of(buf)?;
    let free_end = free_end_of(buf)?;
    let free_start = add(OFF_SLOTS, mul(4, count)?)?;
    // I1
    ensure!(
        free_start <= free_end && free_end <= size,
        "I1: free_start {free_start}, free_end {free_end}, size {size}"
    );
    // I2
    let mut slots = Vec::new();
    let mut live = 0usize;
    for i in 0..count {
        let at = add(OFF_SLOTS, mul(4, i)?)?;
        let offset = usize::from(u16_at(buf, at)?);
        let len = usize::from(u16_at(buf, add(at, 2)?)?);
        ensure!(len >= 1, "I2: slot {i} has len 0");
        ensure!(
            offset >= free_end,
            "I2: slot {i} offset {offset} < {free_end}"
        );
        ensure!(
            add(offset, len)? <= size,
            "I2: slot {i} ends at {} > {size}",
            add(offset, len)?
        );
        live = add(live, len)?;
        slots.push((offset, len));
    }
    // I3
    slots.sort_unstable();
    for pair in slots.windows(2) {
        if let [(offset, len), (next, _)] = pair {
            ensure!(
                add(*offset, *len)? <= *next,
                "I3: cell at {offset} (len {len}) overlaps cell at {next}"
            );
        }
    }
    // I4
    ensure!(
        sub(size, free_end)? == add(live, fragmented)?,
        "I4: {size} - {free_end} != {live} + {fragmented}"
    );
    Ok(())
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

#[test]
fn insert_fails_when_only_fragmented_space_is_free() -> anyhow::Result<()> {
    // Changes in step 9: insert will compact first.
    let mut buf = empty_page(512)?;
    insert(&mut buf, 0, &[0x33; 476])?;
    remove(&mut buf, 0)?;
    let before = buf.clone();
    let error = insert(&mut buf, 0, &[0x44]).err();
    ensure!(
        error
            == Some(PageError::PageFull {
                needed: 5,
                contiguous: 4,
                total_free: 480
            }),
        "got {error:?}"
    );
    ensure!(buf == before, "the failed insert changed the buffer");
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
}
