//! Tests for the B+Tree leaf views (`storage::btree::leaf`, Phase 2 step 2).
//!
//! The format is documented in ADR 0005. These tests check:
//!
//! - `init` writes an empty leaf (page type 4, no next leaf, zero reserved
//!   bytes), and `new` rejects pages of another type.
//! - The bytes of leaf cells (inline, empty value, overflow) and their slots,
//!   at literal offsets.
//! - Insert at the position from `search` keeps the keys in order, and `remove`
//!   takes a cell out. The node does not check the order itself.
//! - The size limits: a key of exactly `max_key_len` works and one more byte
//!   fails, the same for a cell of exactly `max_node_cell_len`, and an empty
//!   key is allowed. A failed insert leaves the page unchanged.
//! - `next_leaf` round-trips at bytes 24..28, and slotted operations never
//!   disturb it.
//! - `validate` finds unordered keys, bad cells and non-zero reserved bytes.
//! - Property tests (page size 512): random keys inserted at their search
//!   position match a sorted `Vec` model, and a cell survives an
//!   encode/decode round trip.
//!
//! Expected values do not come from the code under test: offsets and encoded
//! bytes are written as literals (little-endian).

use std::collections::BTreeMap;

use anyhow::{Context, ensure};
use proptest::prelude::*;
use storage_engine_rs::storage::btree::error::NodeError;
use storage_engine_rs::storage::btree::leaf::{LeafNode, LeafNodeMut};
use storage_engine_rs::storage::btree::node::{
    ValueRef, fits_inline, max_key_len, max_node_cell_len,
};
use storage_engine_rs::storage::pager::error::PageError;
use storage_engine_rs::storage::pager::page::PageType;
use storage_engine_rs::storage::pager::slotted::SlottedPageMut;
use storage_engine_rs::storage::pager::types::{PageId, PageSize};

// Offsets written here independently of the code (ADR 0003 and 0005).
const OFF_TYPE: usize = 4;
const OFF_CELL_COUNT: usize = 16;
const OFF_FREE_END: usize = 20;
const OFF_NEXT_LEAF: usize = 24;
const OFF_RESERVED: usize = 28;
const OFF_SLOTS: usize = 32;

const TYPE_LEAF: u8 = 4;

/// Max cell and max key for a 512-byte page (ADR 0005 table).
const MAX_CELL_512: usize = 116;
const MAX_KEY_512: usize = 105;

fn slice(buf: &[u8], at: usize, len: usize) -> anyhow::Result<&[u8]> {
    let end = at.checked_add(len).context("range overflow")?;
    buf.get(at..end)
        .with_context(|| format!("range {at}..{end} past buffer of {}", buf.len()))
}

fn u16_at(buf: &[u8], at: usize) -> anyhow::Result<u16> {
    Ok(u16::from_le_bytes(slice(buf, at, 2)?.try_into()?))
}

fn u32_at(buf: &[u8], at: usize) -> anyhow::Result<u32> {
    Ok(u32::from_le_bytes(slice(buf, at, 4)?.try_into()?))
}

fn put(buf: &mut [u8], at: usize, bytes: &[u8]) -> anyhow::Result<()> {
    let end = at.checked_add(bytes.len()).context("range overflow")?;
    buf.get_mut(at..end)
        .context("range past buffer")?
        .copy_from_slice(bytes);
    Ok(())
}

/// An empty leaf of `size` bytes, prefilled with 0xAA before `init`.
fn leaf_page(size: usize) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0xAA; size];
    LeafNodeMut::init(&mut buf)?;
    Ok(buf)
}

fn insert(buf: &mut [u8], index: usize, key: &[u8], value: ValueRef<'_>) -> Result<(), NodeError> {
    LeafNodeMut::new(buf)?.insert(index, key, value)
}

/// Inserts `key` at the position `search` gives. Returns `false` if it was
/// already there.
fn upsert_new(buf: &mut [u8], key: &[u8], value: ValueRef<'_>) -> anyhow::Result<bool> {
    let position = LeafNode::new(buf)?.search(key)?;
    match position {
        Ok(_) => Ok(false),
        Err(index) => {
            insert(buf, index, key, value)?;
            Ok(true)
        }
    }
}

/// Every key of the leaf, copied out.
fn keys_of(buf: &[u8]) -> anyhow::Result<Vec<Vec<u8>>> {
    let leaf = LeafNode::new(buf)?;
    (0..leaf.cell_count())
        .map(|i| Ok(leaf.key(i)?.to_vec()))
        .collect()
}

fn ensure_error<T>(
    result: Result<T, NodeError>,
    expected: NodeError,
    what: &str,
) -> anyhow::Result<()> {
    let error = result.err();
    ensure!(
        error == Some(expected),
        "{what}: expected {expected:?}, got {error:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// init and new
// ---------------------------------------------------------------------------

#[test]
fn init_writes_an_empty_leaf() -> anyhow::Result<()> {
    for size in [512usize, 4096, 65536] {
        let buf = leaf_page(size)?;
        ensure!(buf.get(OFF_TYPE) == Some(&TYPE_LEAF), "{size}: type byte");
        ensure!(u16_at(&buf, OFF_CELL_COUNT)? == 0, "{size}: cell_count");
        ensure!(
            usize::try_from(u32_at(&buf, OFF_FREE_END)?)? == size,
            "{size}: free_end"
        );
        ensure!(
            slice(&buf, OFF_NEXT_LEAF, 8)? == [0; 8],
            "{size}: next_leaf and reserved"
        );
        ensure!(
            slice(&buf, 0, 4)? == [0xAA; 4],
            "{size}: checksum untouched"
        );
        let leaf = LeafNode::new(&buf)?;
        ensure!(leaf.cell_count() == 0, "{size}: count");
        ensure!(leaf.next_leaf()?.is_none(), "{size}: next_leaf");
        leaf.validate()?;
    }
    Ok(())
}

#[test]
fn init_rejects_invalid_buffer_lengths_without_writing() -> anyhow::Result<()> {
    let mut buf = vec![0x5C; 513];
    ensure!(LeafNodeMut::init(&mut buf).is_err(), "513 bytes");
    ensure!(buf.iter().all(|&b| b == 0x5C), "init changed the buffer");
    Ok(())
}

#[test]
fn new_requires_a_leaf_page() -> anyhow::Result<()> {
    let mut slotted = vec![0u8; 512];
    SlottedPageMut::init(&mut slotted)?;
    let mut internal = vec![0u8; 512];
    SlottedPageMut::init_as(&mut internal, PageType::BTreeInternal)?;

    for (buf, found) in [
        (slotted, PageType::Slotted),
        (internal, PageType::BTreeInternal),
    ] {
        let expected = NodeError::Page(PageError::UnexpectedPageType {
            expected: PageType::BTreeLeaf,
            found,
        });
        ensure_error(LeafNode::new(&buf), expected, "read view")?;
        let mut copy = buf.clone();
        ensure_error(LeafNodeMut::new(&mut copy), expected, "mut view")?;
        ensure!(copy == buf, "new modified the page");
    }

    let zeroed = vec![0u8; 512];
    ensure_error(
        LeafNode::new(&zeroed),
        NodeError::Page(PageError::UnknownPageType { value: 0 }),
        "zeroed page",
    )?;
    let mut overflow = vec![0u8; 512];
    *overflow.get_mut(OFF_TYPE).context("type byte")? = 6;
    ensure_error(
        LeafNode::new(&overflow),
        NodeError::Page(PageError::UnexpectedPageType {
            expected: PageType::BTreeLeaf,
            found: PageType::Overflow,
        }),
        "overflow page",
    )
}

// ---------------------------------------------------------------------------
// Byte layout of cells
// ---------------------------------------------------------------------------

#[test]
fn inline_cell_bytes() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    insert(&mut buf, 0, b"key", ValueRef::Inline(b"val"))?;
    // key_len = 3, flags = 0, "key", "val": 9 bytes at 503..512.
    ensure!(
        slice(&buf, 503, 9)? == [3, 0, 0, b'k', b'e', b'y', b'v', b'a', b'l'],
        "cell bytes"
    );
    // Slot 0 = [offset 503 = 0x01F7, len 9].
    ensure!(slice(&buf, OFF_SLOTS, 4)? == [0xF7, 0x01, 9, 0], "slot 0");
    ensure!(u16_at(&buf, OFF_CELL_COUNT)? == 1, "cell_count");
    ensure!(u32_at(&buf, OFF_FREE_END)? == 503, "free_end");
    ensure!(slice(&buf, OFF_NEXT_LEAF, 8)? == [0; 8], "header area");

    let leaf = LeafNode::new(&buf)?;
    let cell = leaf.cell(0)?;
    ensure!(cell.key == b"key", "key");
    ensure!(cell.value == ValueRef::Inline(b"val"), "value");
    leaf.validate()?;
    Ok(())
}

#[test]
fn empty_value_cell_bytes() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    insert(&mut buf, 0, b"key", ValueRef::Inline(b""))?;
    // 6 bytes at 506..512.
    ensure!(
        slice(&buf, 506, 6)? == [3, 0, 0, b'k', b'e', b'y'],
        "cell bytes"
    );
    ensure!(
        LeafNode::new(&buf)?.cell(0)?.value == ValueRef::Inline(b""),
        "value"
    );
    Ok(())
}

#[test]
fn overflow_cell_bytes() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    let value = ValueRef::Overflow {
        len: 100_000,
        first_page: PageId::new(7),
    };
    insert(&mut buf, 0, b"k", value)?;
    // key_len = 1, flags = 1, "k", value_len = 0x000186A0, first = 7: 12 bytes
    // at 500..512.
    ensure!(
        slice(&buf, 500, 12)? == [1, 0, 1, b'k', 0xA0, 0x86, 0x01, 0x00, 7, 0, 0, 0],
        "cell bytes"
    );
    ensure!(LeafNode::new(&buf)?.cell(0)?.value == value, "value");
    Ok(())
}

#[test]
fn zero_first_overflow_page_is_rejected() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    let before = buf.clone();
    let value = ValueRef::Overflow {
        len: 10,
        first_page: PageId::new(0),
    };
    ensure!(
        matches!(
            insert(&mut buf, 0, b"k", value),
            Err(NodeError::CorruptNode { .. })
        ),
        "page 0"
    );
    ensure!(buf == before, "the page changed");
    Ok(())
}

// ---------------------------------------------------------------------------
// Insert, search and remove
// ---------------------------------------------------------------------------

#[test]
fn search_on_an_empty_leaf_is_err_0() -> anyhow::Result<()> {
    let buf = leaf_page(512)?;
    ensure!(LeafNode::new(&buf)?.search(b"x")? == Err(0), "search");
    Ok(())
}

#[test]
fn insert_at_search_position_keeps_order() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    for key in [&b"delta"[..], b"alpha", b"echo", b"bravo", b"charlie"] {
        ensure!(
            upsert_new(&mut buf, key, ValueRef::Inline(key))?,
            "duplicate"
        );
    }
    ensure!(
        keys_of(&buf)?
            == [&b"alpha"[..], b"bravo", b"charlie", b"delta", b"echo"]
                .iter()
                .map(|k| k.to_vec())
                .collect::<Vec<_>>(),
        "order"
    );

    let leaf = LeafNode::new(&buf)?;
    ensure!(leaf.search(b"alpha")? == Ok(0), "first");
    ensure!(leaf.search(b"charlie")? == Ok(2), "middle");
    ensure!(leaf.search(b"echo")? == Ok(4), "last");
    ensure!(leaf.search(b"a")? == Err(0), "before all");
    ensure!(leaf.search(b"alphb")? == Err(1), "between");
    ensure!(leaf.search(b"zulu")? == Err(5), "after all");
    // memcmp order: a prefix sorts before the longer key.
    ensure!(leaf.search(b"brav")? == Err(1), "prefix of a key");
    ensure!(leaf.search(b"bravoo")? == Err(2), "extension of a key");
    leaf.validate()?;
    Ok(())
}

#[test]
fn keys_compare_as_unsigned_bytes() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    for key in [&[0x80u8][..], &[0x7F], &[0xFF], &[0x00]] {
        ensure!(upsert_new(&mut buf, key, ValueRef::Inline(b""))?, "dup");
    }
    ensure!(
        keys_of(&buf)? == vec![vec![0x00], vec![0x7F], vec![0x80], vec![0xFF]],
        "order"
    );
    Ok(())
}

#[test]
fn remove_takes_a_cell_out_and_keeps_order() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    for key in [&b"a"[..], b"b", b"c", b"d"] {
        upsert_new(&mut buf, key, ValueRef::Inline(b"v"))?;
    }
    LeafNodeMut::new(&mut buf)?.remove(1)?;
    ensure!(
        keys_of(&buf)? == vec![b"a".to_vec(), b"c".to_vec(), b"d".to_vec()],
        "after remove"
    );
    let leaf = LeafNode::new(&buf)?;
    ensure!(leaf.search(b"b")? == Err(1), "removed key");
    ensure!(leaf.search(b"c")? == Ok(1), "shifted key");
    leaf.validate()?;

    ensure!(
        matches!(
            LeafNodeMut::new(&mut buf)?.remove(3),
            Err(NodeError::Page(PageError::SlotOutOfRange { index: 3, .. }))
        ),
        "remove past the end"
    );
    Ok(())
}

#[test]
fn cell_and_key_out_of_range_are_page_errors() -> anyhow::Result<()> {
    let buf = leaf_page(512)?;
    let leaf = LeafNode::new(&buf)?;
    let expected = NodeError::Page(PageError::SlotOutOfRange {
        index: 0,
        cell_count: 0,
    });
    ensure_error(leaf.cell(0), expected, "cell")?;
    ensure_error(leaf.key(0), expected, "key")
}

#[test]
fn views_borrow_from_the_page() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    insert(&mut buf, 0, b"key", ValueRef::Inline(b"val"))?;
    let range = buf.as_ptr_range();
    let leaf = LeafNode::new(&buf)?;
    let cell = leaf.cell(0)?;
    ensure!(range.contains(&cell.key.as_ptr()), "key");
    ensure!(range.contains(&leaf.key(0)?.as_ptr()), "key()");
    Ok(())
}

#[test]
fn empty_key_is_allowed_and_sorts_first() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    upsert_new(&mut buf, b"a", ValueRef::Inline(b"1"))?;
    upsert_new(&mut buf, b"", ValueRef::Inline(b"0"))?;
    ensure!(keys_of(&buf)? == vec![vec![], b"a".to_vec()], "order");
    let leaf = LeafNode::new(&buf)?;
    ensure!(leaf.search(b"")? == Ok(0), "search empty");
    ensure!(leaf.cell(0)?.value == ValueRef::Inline(b"0"), "value");
    leaf.validate()?;
    // Cell bytes: key_len 0, flags 0, value "0" (4 bytes) at 503, slot 0.
    ensure!(
        slice(&buf, 32, 4)? == [0xF7, 0x01, 4, 0],
        "slot of the empty-key cell"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Size limits
// ---------------------------------------------------------------------------

#[test]
fn key_of_max_len_works_and_one_more_byte_fails() -> anyhow::Result<()> {
    let size = PageSize::new(512)?;
    ensure!(max_key_len(size) == MAX_KEY_512, "fixture");
    let mut buf = leaf_page(512)?;

    let max_key = vec![b'k'; MAX_KEY_512];
    let overflow = ValueRef::Overflow {
        len: 5000,
        first_page: PageId::new(3),
    };
    insert(&mut buf, 0, &max_key, overflow)?;
    ensure!(LeafNode::new(&buf)?.cell(0)?.key == max_key, "stored");

    let mut other = leaf_page(512)?;
    insert(&mut other, 0, &max_key, ValueRef::Inline(b"12345678"))?;
    LeafNode::new(&other)?.validate()?;

    let too_long = vec![b'k'; MAX_KEY_512 + 1];
    let mut fresh = leaf_page(512)?;
    let before = fresh.clone();
    ensure_error(
        insert(&mut fresh, 0, &too_long, overflow),
        NodeError::KeyTooLarge {
            len: MAX_KEY_512 + 1,
            max: MAX_KEY_512,
        },
        "key + 1",
    )?;
    ensure!(fresh == before, "a rejected key changed the page");
    Ok(())
}

#[test]
fn key_limits_at_other_page_sizes() -> anyhow::Result<()> {
    for (size, max_key) in [(4096usize, 1001usize), (65536, 16361)] {
        let mut buf = leaf_page(size)?;
        let key = vec![b'k'; max_key];
        let overflow = ValueRef::Overflow {
            len: 1,
            first_page: PageId::new(2),
        };
        insert(&mut buf, 0, &key, overflow)?;
        let too_long = vec![b'k'; max_key + 1];
        ensure!(
            matches!(
                insert(&mut buf, 0, &too_long, overflow),
                Err(NodeError::KeyTooLarge { .. })
            ),
            "{size}: key + 1"
        );
    }
    Ok(())
}

#[test]
fn inline_cell_of_max_len_works_and_one_more_byte_fails() -> anyhow::Result<()> {
    let page_size = PageSize::new(512)?;
    ensure!(max_node_cell_len(page_size) == MAX_CELL_512, "fixture");
    let key = [b'k'; 10];
    // 3 + 10 + 103 = 116.
    let exact = vec![0x55; 103];
    ensure!(fits_inline(page_size, 10, 103), "fits_inline agrees");
    let mut buf = leaf_page(512)?;
    insert(&mut buf, 0, &key, ValueRef::Inline(&exact))?;
    ensure!(
        LeafNode::new(&buf)?.cell(0)?.value == ValueRef::Inline(&exact),
        "stored"
    );
    LeafNode::new(&buf)?.validate()?;

    let over = vec![0x55; 104];
    ensure!(!fits_inline(page_size, 10, 104), "fits_inline agrees");
    let mut fresh = leaf_page(512)?;
    let before = fresh.clone();
    ensure_error(
        insert(&mut fresh, 0, &key, ValueRef::Inline(&over)),
        NodeError::CellTooLarge {
            len: MAX_CELL_512 + 1,
            max: MAX_CELL_512,
        },
        "cell + 1",
    )?;
    ensure!(fresh == before, "a rejected cell changed the page");
    Ok(())
}

#[test]
fn insert_errors_leave_the_page_unchanged() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    // 8-byte keys with 40-byte values: 51-byte cells + 4-byte slots = 55; the
    // 480 usable bytes hold 8 of them.
    let value = [0x42u8; 40];
    for i in 0u8..8 {
        let key = [i; 8];
        insert(&mut buf, usize::from(i), &key, ValueRef::Inline(&value))?;
    }
    let before = buf.clone();

    let full = insert(&mut buf, 8, &[9; 8], ValueRef::Inline(&value));
    ensure!(
        matches!(full, Err(NodeError::Page(PageError::PageFull { .. }))),
        "page full: {full:?}"
    );
    ensure!(buf == before, "PageFull changed the page");

    let past = insert(&mut buf, 9, &[9; 8], ValueRef::Inline(b""));
    ensure!(
        matches!(
            past,
            Err(NodeError::Page(PageError::SlotOutOfRange { index: 9, .. }))
        ),
        "index past count: {past:?}"
    );
    ensure!(buf == before, "SlotOutOfRange changed the page");
    LeafNode::new(&buf)?.validate()?;
    Ok(())
}

#[test]
fn insert_compacts_when_only_fragmented_space_is_left() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    let value = [0x42u8; 40];
    for i in 0u8..8 {
        insert(&mut buf, usize::from(i), &[i; 8], ValueRef::Inline(&value))?;
    }
    LeafNodeMut::new(&mut buf)?.remove(3)?;
    let page = LeafNode::new(&buf)?;
    ensure!(page.slotted().fragmented_bytes() == 51, "fixture");
    insert(&mut buf, 3, &[3; 8], ValueRef::Inline(&value))?;
    let page = LeafNode::new(&buf)?;
    ensure!(page.cell_count() == 8, "count");
    ensure!(page.slotted().fragmented_bytes() == 0, "compacted");
    page.validate()?;
    Ok(())
}

#[test]
fn slotted_view_reports_free_space() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    ensure!(
        LeafNode::new(&buf)?.slotted().contiguous_free() == 480,
        "empty leaf"
    );
    insert(&mut buf, 0, b"key", ValueRef::Inline(b"val"))?;
    let leaf = LeafNode::new(&buf)?;
    // 480 - (9 + 4).
    ensure!(leaf.slotted().total_free() == 467, "after one insert");
    Ok(())
}

// ---------------------------------------------------------------------------
// next_leaf
// ---------------------------------------------------------------------------

#[test]
fn next_leaf_round_trips_at_bytes_24_to_28() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    LeafNodeMut::new(&mut buf)?.set_next_leaf(Some(PageId::new(0x0102_0304)))?;
    ensure!(slice(&buf, 24, 4)? == [4, 3, 2, 1], "little-endian");
    ensure!(slice(&buf, OFF_RESERVED, 4)? == [0; 4], "reserved");
    ensure!(
        LeafNode::new(&buf)?.next_leaf()? == Some(PageId::new(0x0102_0304)),
        "read back"
    );

    LeafNodeMut::new(&mut buf)?.set_next_leaf(None)?;
    ensure!(slice(&buf, 24, 8)? == [0; 8], "cleared");
    ensure!(LeafNode::new(&buf)?.next_leaf()?.is_none(), "none");
    Ok(())
}

#[test]
fn set_next_leaf_changes_only_bytes_24_to_28() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    insert(&mut buf, 0, b"key", ValueRef::Inline(b"val"))?;
    let before = buf.clone();
    LeafNodeMut::new(&mut buf)?.set_next_leaf(Some(PageId::new(77)))?;
    for (at, (new, old)) in buf.iter().zip(&before).enumerate() {
        ensure!(
            (24..28).contains(&at) || new == old,
            "byte {at} changed outside the link"
        );
    }
    Ok(())
}

#[test]
fn slotted_operations_do_not_disturb_next_leaf() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    LeafNodeMut::new(&mut buf)?.set_next_leaf(Some(PageId::new(42)))?;
    let value = [0x42u8; 40];
    for i in 0u8..8 {
        insert(&mut buf, usize::from(i), &[i; 8], ValueRef::Inline(&value))?;
    }
    ensure!(
        LeafNode::new(&buf)?.next_leaf()? == Some(PageId::new(42)),
        "insert"
    );
    LeafNodeMut::new(&mut buf)?.remove(2)?;
    ensure!(
        LeafNode::new(&buf)?.next_leaf()? == Some(PageId::new(42)),
        "remove"
    );
    // This insert compacts first.
    insert(&mut buf, 2, &[2; 8], ValueRef::Inline(&value))?;
    ensure!(
        LeafNode::new(&buf)?.next_leaf()? == Some(PageId::new(42)),
        "insert with compaction"
    );
    ensure!(slice(&buf, OFF_RESERVED, 4)? == [0; 4], "reserved");
    LeafNode::new(&buf)?.validate()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// validate
// ---------------------------------------------------------------------------

fn validate_fails(buf: &[u8], what: &str) -> anyhow::Result<()> {
    let outcome = LeafNode::new(buf).and_then(|leaf| leaf.validate());
    ensure!(outcome.is_err(), "{what}: validate passed");
    Ok(())
}

#[test]
fn validate_rejects_unordered_and_duplicate_keys() -> anyhow::Result<()> {
    // The node does not check the order, so the caller can break it.
    let mut buf = leaf_page(512)?;
    insert(&mut buf, 0, b"b", ValueRef::Inline(b""))?;
    insert(&mut buf, 1, b"a", ValueRef::Inline(b""))?;
    validate_fails(&buf, "descending keys")?;

    let mut buf = leaf_page(512)?;
    insert(&mut buf, 0, b"a", ValueRef::Inline(b"1"))?;
    insert(&mut buf, 1, b"a", ValueRef::Inline(b"2"))?;
    validate_fails(&buf, "duplicate keys")
}

#[test]
fn validate_rejects_bad_cells_and_reserved_bytes() -> anyhow::Result<()> {
    let mut base = leaf_page(512)?;
    insert(&mut base, 0, b"a", ValueRef::Inline(b"v"))?;
    LeafNode::new(&base)?.validate()?;

    // The cell is at 507..512: key_len (2), flags (1), key, value.
    let mut flags = base.clone();
    put(&mut flags, 509, &[0x02])?;
    validate_fails(&flags, "unknown flag bit")?;

    let mut key_len = base.clone();
    put(&mut key_len, 507, &[9, 0])?;
    validate_fails(&key_len, "key_len past the cell")?;

    let mut reserved = base.clone();
    put(&mut reserved, OFF_RESERVED, &[0, 0, 0, 1])?;
    validate_fails(&reserved, "non-zero reserved bytes")?;

    let mut overflow = leaf_page(512)?;
    insert(
        &mut overflow,
        0,
        b"a",
        ValueRef::Overflow {
            len: 9,
            first_page: PageId::new(5),
        },
    )?;
    LeafNode::new(&overflow)?.validate()?;
    // The overflow cell is at 500..512; first_overflow is at 508..512.
    put(&mut overflow, 508, &[0, 0, 0, 0])?;
    validate_fails(&overflow, "first overflow page 0")?;

    let mut slots = base;
    put(&mut slots, OFF_SLOTS, &[0, 0, 0, 0])?;
    validate_fails(&slots, "corrupt slot")
}

#[test]
fn reading_a_bad_cell_is_corrupt_node() -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    insert(&mut buf, 0, b"a", ValueRef::Inline(b"v"))?;
    put(&mut buf, 509, &[0x80])?;
    let leaf = LeafNode::new(&buf)?;
    ensure!(
        matches!(leaf.cell(0), Err(NodeError::CorruptNode { .. })),
        "cell"
    );
    // The key alone is still readable.
    ensure!(leaf.key(0)? == b"a", "key");
    Ok(())
}

// ---------------------------------------------------------------------------
// Property tests (page size 512)
// ---------------------------------------------------------------------------

/// Inserts every key at its search position until the page is full, and checks
/// the leaf against a sorted model after each insert.
fn run_model(keys: Vec<Vec<u8>>) -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    for key in keys {
        let value: Vec<u8> = key.iter().rev().copied().collect();
        let position = LeafNode::new(&buf)?.search(&key)?;
        let Err(index) = position else {
            ensure!(model.contains_key(&key), "search found a missing key");
            continue;
        };
        match insert(&mut buf, index, &key, ValueRef::Inline(&value)) {
            Ok(()) => {
                model.insert(key, value);
            }
            Err(NodeError::Page(PageError::PageFull { .. })) => break,
            Err(error) => return Err(error.into()),
        }

        let leaf = LeafNode::new(&buf)?;
        leaf.validate()?;
        ensure!(leaf.cell_count() == model.len(), "cell count");
    }

    let leaf = LeafNode::new(&buf)?;
    for (index, (key, value)) in model.iter().enumerate() {
        let cell = leaf.cell(index)?;
        ensure!(cell.key == key.as_slice(), "key at {index}");
        ensure!(cell.value == ValueRef::Inline(value), "value at {index}");
        ensure!(leaf.search(key)? == Ok(index), "search of {index}");
    }
    Ok(())
}

fn round_trip(key: &[u8], value: ValueRef<'_>) -> anyhow::Result<()> {
    let mut buf = leaf_page(512)?;
    insert(&mut buf, 0, key, value)?;
    let leaf = LeafNode::new(&buf)?;
    let cell = leaf.cell(0)?;
    ensure!(cell.key == key, "key");
    ensure!(cell.value == value, "value");
    leaf.validate()?;
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_inserts_at_search_position_match_a_sorted_model(
        keys in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..=12), 1..40)
    ) {
        run_model(keys).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }

    #[test]
    fn prop_inline_cell_round_trips(
        key in prop::collection::vec(any::<u8>(), 0..=MAX_KEY_512),
        value in prop::collection::vec(any::<u8>(), 0..=MAX_CELL_512),
    ) {
        let value_len = MAX_CELL_512.saturating_sub(3).saturating_sub(key.len());
        let value = value.get(..value_len.min(value.len())).unwrap_or(&[]);
        round_trip(&key, ValueRef::Inline(value))
            .map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }

    #[test]
    fn prop_overflow_cell_round_trips(
        key in prop::collection::vec(any::<u8>(), 0..=MAX_KEY_512),
        len: u32,
        first in 1u32..,
    ) {
        let value = ValueRef::Overflow { len, first_page: PageId::new(first) };
        round_trip(&key, value).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
