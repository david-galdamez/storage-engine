//! Tests for the node limits and the leaf cell codec (`storage::btree::node`,
//! Phase 2 step 2).
//!
//! The formats are documented in ADR 0005. These tests check:
//!
//! - The limits (`max_node_cell_len`, `max_key_len`) for 512, 4096 and 65536,
//!   against the values written in the ADR, and `fits_inline` at its
//!   boundaries, including inputs that would overflow a `usize`.
//! - `leaf_cell_len` for inline and overflow values.
//! - `decode_leaf_cell` on cells written byte by byte, and one test per
//!   corruption it must reject.
//!
//! Expected values do not come from the code under test: limits and cells are
//! written as literals (little-endian).

use anyhow::ensure;
use storage_engine_rs::storage::btree::error::NodeError;
use storage_engine_rs::storage::btree::node::{
    LeafCell, ValueRef, decode_leaf_cell, fits_inline, leaf_cell_len, max_key_len,
    max_node_cell_len,
};
use storage_engine_rs::storage::pager::types::{PageId, PageSize};

fn size(bytes: u32) -> anyhow::Result<PageSize> {
    Ok(PageSize::new(bytes)?)
}

fn corrupt(cell: &[u8]) -> Option<&'static str> {
    match decode_leaf_cell(cell) {
        Err(NodeError::CorruptNode { reason }) => Some(reason),
        _ => None,
    }
}

#[test]
fn limits_match_the_adr_table() -> anyhow::Result<()> {
    for (page_size, max_cell, max_key) in
        [(512, 116, 105), (4096, 1012, 1001), (65536, 16372, 16361)]
    {
        let page_size = size(page_size)?;
        ensure!(
            max_node_cell_len(page_size) == max_cell,
            "{} max_cell",
            page_size.get()
        );
        ensure!(
            max_key_len(page_size) == max_key,
            "{} max_key",
            page_size.get()
        );
    }
    Ok(())
}

#[test]
fn a_max_size_entry_uses_a_quarter_of_the_usable_space() -> anyhow::Result<()> {
    // Cell plus its 4-byte slot is (page_size - 32) / 4.
    for page_size in [512u32, 1024, 2048, 4096, 8192, 16384, 32768, 65536] {
        let max_cell = max_node_cell_len(size(page_size)?);
        let usable = usize::try_from(page_size)?.saturating_sub(32);
        ensure!(
            max_cell.checked_add(4) == usable.checked_div(4),
            "{page_size}: {max_cell}"
        );
    }
    Ok(())
}

#[test]
fn fits_inline_boundaries_at_512() -> anyhow::Result<()> {
    let s = size(512)?;
    // 3 + key + value <= 116.
    ensure!(fits_inline(s, 0, 113), "empty key, 113 bytes");
    ensure!(!fits_inline(s, 0, 114), "empty key, 114 bytes");
    ensure!(fits_inline(s, 10, 103), "10-byte key, 103 bytes");
    ensure!(!fits_inline(s, 10, 104), "10-byte key, 104 bytes");
    ensure!(fits_inline(s, 105, 8), "max key, 8 bytes");
    ensure!(!fits_inline(s, 105, 9), "max key, 9 bytes");
    ensure!(fits_inline(s, 10, 0), "empty value");
    Ok(())
}

#[test]
fn fits_inline_does_not_overflow_on_huge_inputs() -> anyhow::Result<()> {
    let s = size(65536)?;
    ensure!(!fits_inline(s, usize::MAX, 1), "key + value overflows");
    ensure!(!fits_inline(s, 1, usize::MAX), "value + key overflows");
    ensure!(!fits_inline(s, usize::MAX, usize::MAX), "both huge");
    ensure!(
        !fits_inline(s, usize::MAX.saturating_sub(1), 1),
        "sum + 3 overflows"
    );
    ensure!(!fits_inline(s, 0, usize::MAX), "huge value");
    Ok(())
}

#[test]
fn leaf_cell_len_counts_header_key_and_value() {
    assert_eq!(leaf_cell_len(3, &ValueRef::Inline(b"")), 6);
    assert_eq!(leaf_cell_len(3, &ValueRef::Inline(b"hello")), 11);
    let overflow = ValueRef::Overflow {
        len: 1_000_000,
        first_page: PageId::new(9),
    };
    assert_eq!(leaf_cell_len(0, &overflow), 11);
    assert_eq!(leaf_cell_len(3, &overflow), 14);
    assert_eq!(
        leaf_cell_len(usize::MAX, &ValueRef::Inline(b"x")),
        usize::MAX
    );
}

#[test]
fn decode_inline_cell() -> anyhow::Result<()> {
    // key_len = 2, flags = 0, key "ab", value "xyz".
    let cell = [2, 0, 0, b'a', b'b', b'x', b'y', b'z'];
    let decoded = decode_leaf_cell(&cell)?;
    ensure!(
        decoded
            == LeafCell {
                key: b"ab",
                value: ValueRef::Inline(b"xyz")
            },
        "{decoded:?}"
    );
    Ok(())
}

#[test]
fn decode_inline_cell_with_empty_value_and_empty_key() -> anyhow::Result<()> {
    let empty_value = decode_leaf_cell(&[2, 0, 0, b'a', b'b'])?;
    ensure!(empty_value.value == ValueRef::Inline(b""), "empty value");
    ensure!(empty_value.key == b"ab", "key");

    let empty_key = decode_leaf_cell(&[0, 0, 0, b'v'])?;
    ensure!(empty_key.key.is_empty(), "empty key");
    ensure!(empty_key.value == ValueRef::Inline(b"v"), "value");

    let nothing = decode_leaf_cell(&[0, 0, 0])?;
    ensure!(nothing.key.is_empty(), "empty key and value, key");
    ensure!(
        nothing.value == ValueRef::Inline(b""),
        "empty key and value"
    );
    Ok(())
}

#[test]
fn decode_overflow_cell() -> anyhow::Result<()> {
    // key_len = 1, flags = 1, key "k", value_len = 100000 (0x000186A0),
    // first_overflow = 7.
    let cell = [1, 0, 1, b'k', 0xA0, 0x86, 0x01, 0x00, 7, 0, 0, 0];
    let decoded = decode_leaf_cell(&cell)?;
    ensure!(decoded.key == b"k", "key");
    ensure!(
        decoded.value
            == ValueRef::Overflow {
                len: 100_000,
                first_page: PageId::new(7)
            },
        "{:?}",
        decoded.value
    );
    Ok(())
}

#[test]
fn decode_rejects_cells_shorter_than_the_header() {
    assert!(corrupt(&[]).is_some(), "empty");
    assert!(corrupt(&[1]).is_some(), "1 byte");
    assert!(corrupt(&[1, 0]).is_some(), "2 bytes");
}

#[test]
fn decode_rejects_a_key_past_the_end() {
    // key_len = 5 but only 2 key bytes.
    assert!(corrupt(&[5, 0, 0, b'a', b'b']).is_some(), "inline");
    // key_len = 0xFFFF.
    assert!(corrupt(&[0xFF, 0xFF, 0, b'a']).is_some(), "huge key_len");
    // Same for an overflow cell.
    assert!(corrupt(&[5, 0, 1, b'a', b'b']).is_some(), "overflow");
}

#[test]
fn decode_rejects_unknown_flag_bits() {
    for flags in [2u8, 3, 4, 0x80, 0xFF] {
        assert!(
            corrupt(&[1, 0, flags, b'k', 0, 0, 0, 0, 1, 0, 0, 0]).is_some(),
            "flags {flags:#x}"
        );
        assert!(
            corrupt(&[1, 0, flags, b'k', b'v']).is_some(),
            "flags {flags:#x}"
        );
    }
}

#[test]
fn decode_rejects_an_overflow_cell_of_the_wrong_length() {
    // 11 + key_len = 12 bytes is the only valid length for a 1-byte key.
    let valid = [1, 0, 1, b'k', 5, 0, 0, 0, 7, 0, 0, 0];
    assert!(decode_leaf_cell(&valid).is_ok(), "fixture");
    for len in [3usize, 4, 8, 10, 11] {
        assert!(
            corrupt(valid.get(..len).unwrap_or(&[])).is_some(),
            "{len} bytes (too short)"
        );
    }
    let mut longer = valid.to_vec();
    longer.push(0);
    assert!(corrupt(&longer).is_some(), "13 bytes (too long)");
}

#[test]
fn decode_rejects_an_overflow_cell_with_first_page_zero() {
    let cell = [1, 0, 1, b'k', 5, 0, 0, 0, 0, 0, 0, 0];
    assert!(corrupt(&cell).is_some());
}

#[test]
fn decode_borrows_from_the_cell() -> anyhow::Result<()> {
    let cell = [1, 0, 0, b'k', b'v', b'v'];
    let decoded = decode_leaf_cell(&cell)?;
    let range = cell.as_ptr_range();
    ensure!(
        range.contains(&decoded.key.as_ptr()),
        "the key is a slice of the cell"
    );
    if let ValueRef::Inline(value) = decoded.value {
        ensure!(
            range.contains(&value.as_ptr()),
            "the value is a slice of the cell"
        );
    }
    Ok(())
}
