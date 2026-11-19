//! Raw-byte checkers shared by the pager integration tests.
//!
//! The checks read the page bytes directly, with offsets written down
//! independently of the code under test (ADR 0003), so they do not depend on
//! the API they verify. Each test binary that includes this module with
//! `mod common;` must use every item, or `dead_code` fires.

use anyhow::{Context, ensure};
use storage_engine_rs::storage::pager::slotted::{SlottedPage, SlottedPageMut};

// Offsets of the slotted page header (ADR 0003).
const OFF_CELL_COUNT: usize = 16;
const OFF_FRAGMENTED: usize = 18;
const OFF_FREE_END: usize = 20;
const OFF_SLOTS: usize = 32;

pub fn add(a: usize, b: usize) -> anyhow::Result<usize> {
    a.checked_add(b).context("add overflow")
}

pub fn sub(a: usize, b: usize) -> anyhow::Result<usize> {
    a.checked_sub(b).context("sub underflow")
}

pub fn mul(a: usize, b: usize) -> anyhow::Result<usize> {
    a.checked_mul(b).context("mul overflow")
}

/// `len` bytes of `buf` starting at `at`.
pub fn slice(buf: &[u8], at: usize, len: usize) -> anyhow::Result<&[u8]> {
    let end = add(at, len)?;
    buf.get(at..end)
        .with_context(|| format!("range {at}..{end} past buffer of {}", buf.len()))
}

pub fn u16_at(buf: &[u8], at: usize) -> anyhow::Result<u16> {
    Ok(u16::from_le_bytes(slice(buf, at, 2)?.try_into()?))
}

pub fn u32_at(buf: &[u8], at: usize) -> anyhow::Result<u32> {
    Ok(u32::from_le_bytes(slice(buf, at, 4)?.try_into()?))
}

pub fn count_of(buf: &[u8]) -> anyhow::Result<usize> {
    Ok(usize::from(u16_at(buf, OFF_CELL_COUNT)?))
}

pub fn fragmented_of(buf: &[u8]) -> anyhow::Result<usize> {
    Ok(usize::from(u16_at(buf, OFF_FRAGMENTED)?))
}

pub fn free_end_of(buf: &[u8]) -> anyhow::Result<usize> {
    Ok(usize::try_from(u32_at(buf, OFF_FREE_END)?)?)
}

/// A page of `size` bytes, prefilled with 0xAA and then initialized, so any
/// byte `init` forgets to write shows up.
pub fn empty_page(size: usize) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0xAA; size];
    SlottedPageMut::init(&mut buf)?;
    Ok(buf)
}

/// `free_start`, computed from the raw `cell_count`.
pub fn free_start_of(buf: &[u8]) -> anyhow::Result<usize> {
    add(OFF_SLOTS, mul(4, count_of(buf)?)?)
}

/// Every cell of the page, in logical order, copied out.
pub fn cells_of(buf: &[u8]) -> anyhow::Result<Vec<Vec<u8>>> {
    let page = SlottedPage::new(buf)?;
    (0..page.cell_count())
        .map(|i| Ok(page.cell(i)?.to_vec()))
        .collect()
}

/// The free gap `[free_start, free_end)` must be all zeros: `init` zeroes it,
/// `remove` zeroes the slot it frees, and `compact` zeroes the bytes it vacates.
pub fn check_free_gap_zeroed(buf: &[u8]) -> anyhow::Result<()> {
    let free_start = free_start_of(buf)?;
    let free_end = free_end_of(buf)?;
    let gap = slice(buf, free_start, sub(free_end, free_start)?)?;
    let first = gap.iter().position(|&b| b != 0);
    ensure!(
        first.is_none(),
        "free gap [{free_start}, {free_end}) has a non-zero byte at {:?}",
        first.map(|at| free_start.saturating_add(at))
    );
    Ok(())
}

/// I1 to I4 hold, `fragmented_bytes` is 0 and the free gap is zeroed. With I3
/// and I4 this means the live cells fill `[free_end, size)` exactly, with no
/// holes, in any physical order. This is the state after an insert that had
/// to compact: the new cell sits at `free_end` whatever its logical index.
pub fn check_no_holes(buf: &[u8]) -> anyhow::Result<()> {
    check_invariants(buf)?;
    let fragmented = fragmented_of(buf)?;
    ensure!(
        fragmented == 0,
        "fragmented_bytes is {fragmented}, expected 0"
    );
    check_free_gap_zeroed(buf)
}

/// The layout right after `compact`, from the raw bytes: no holes, and the
/// cells in logical order from low to high addresses. Cell 0 starts at
/// `free_end`, each next cell starts where the previous one ends, and the
/// last cell ends at the page end.
pub fn check_compacted(buf: &[u8]) -> anyhow::Result<()> {
    check_no_holes(buf)?;
    let mut expected_offset = free_end_of(buf)?;
    for i in 0..count_of(buf)? {
        let at = add(OFF_SLOTS, mul(4, i)?)?;
        let offset = usize::from(u16_at(buf, at)?);
        let len = usize::from(u16_at(buf, add(at, 2)?)?);
        ensure!(
            offset == expected_offset,
            "slot {i}: offset {offset}, expected {expected_offset} (logical order)"
        );
        expected_offset = add(offset, len)?;
    }
    ensure!(
        expected_offset == buf.len(),
        "the last cell ends at {expected_offset}, not at the page end"
    );
    Ok(())
}

/// Checks the invariants I1 to I4 of ADR 0003 from the raw bytes, without
/// using the API under test. I3 (no overlap) sorts the slots by offset.
pub fn check_invariants(buf: &[u8]) -> anyhow::Result<()> {
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
