//! Tests for the B+Tree internal node views (`storage::btree::internal`,
//! Phase 2 step 2).
//!
//! The format is documented in ADR 0005. An internal node has N keys and N + 1
//! children: N cells `[child: u32][key]`, and the rightmost child at bytes
//! 24..28. These tests check:
//!
//! - `init` writes the rightmost child and zero reserved bytes, and rejects
//!   page 0. `new` rejects pages of another type.
//! - The bytes of cells and of the rightmost pointer, at literal offsets.
//! - `child_for` at its boundaries (invariant 4): a key below the first
//!   separator, equal to a separator (goes right), between separators, above
//!   the last one, and an empty node with only a rightmost child.
//! - `child_at` and `set_child_at` for every position 0..=N, and the errors
//!   for position N + 1 and for page 0.
//! - Key limits, `remove`, slotted operations that never disturb the rightmost
//!   child, and `validate` on broken nodes.
//! - A property test (page size 512): random keys inserted at their search
//!   position match a sorted `Vec` model, including `child_for`.
//!
//! Expected values do not come from the code under test: offsets and encoded
//! bytes are written as literals (little-endian).

use anyhow::{Context, ensure};
use proptest::prelude::*;
use storage_engine_rs::storage::btree::error::NodeError;
use storage_engine_rs::storage::btree::internal::{InternalNode, InternalNodeMut};
use storage_engine_rs::storage::btree::leaf::LeafNodeMut;
use storage_engine_rs::storage::pager::error::PageError;
use storage_engine_rs::storage::pager::page::PageType;
use storage_engine_rs::storage::pager::slotted::SlottedPageMut;
use storage_engine_rs::storage::pager::types::PageId;

// Offsets written here independently of the code (ADR 0003 and 0005).
const OFF_TYPE: usize = 4;
const OFF_CELL_COUNT: usize = 16;
const OFF_FREE_END: usize = 20;
const OFF_RIGHTMOST: usize = 24;
const OFF_RESERVED: usize = 28;
const OFF_SLOTS: usize = 32;

const TYPE_INTERNAL: u8 = 5;

/// Max key for a 512-byte page (ADR 0005 table).
const MAX_KEY_512: usize = 105;

const fn pid(id: u32) -> PageId {
    PageId::new(id)
}

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

/// An empty internal node of `size` bytes with `rightmost`, prefilled with
/// 0xAA before `init`.
fn node_page(size: usize, rightmost: u32) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0xAA; size];
    InternalNodeMut::init(&mut buf, pid(rightmost))?;
    Ok(buf)
}

fn insert(buf: &mut [u8], index: usize, key: &[u8], child: u32) -> Result<(), NodeError> {
    InternalNodeMut::new(buf)?.insert(index, key, pid(child))
}

/// A node with keys b, d, f, children 1, 2, 3 and rightmost child 4.
fn bdf_node() -> anyhow::Result<Vec<u8>> {
    let mut buf = node_page(512, 4)?;
    insert(&mut buf, 0, b"b", 1)?;
    insert(&mut buf, 1, b"d", 2)?;
    insert(&mut buf, 2, b"f", 3)?;
    InternalNode::new(&buf)?.validate()?;
    Ok(buf)
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
fn init_writes_the_rightmost_child() -> anyhow::Result<()> {
    for size in [512usize, 4096, 65536] {
        let buf = node_page(size, 0x0A0B_0C0D)?;
        ensure!(
            buf.get(OFF_TYPE) == Some(&TYPE_INTERNAL),
            "{size}: type byte"
        );
        ensure!(u16_at(&buf, OFF_CELL_COUNT)? == 0, "{size}: cell_count");
        ensure!(
            usize::try_from(u32_at(&buf, OFF_FREE_END)?)? == size,
            "{size}: free_end"
        );
        ensure!(
            slice(&buf, OFF_RIGHTMOST, 4)? == [0x0D, 0x0C, 0x0B, 0x0A],
            "{size}: rightmost, little-endian"
        );
        ensure!(slice(&buf, OFF_RESERVED, 4)? == [0; 4], "{size}: reserved");
        ensure!(
            slice(&buf, 0, 4)? == [0xAA; 4],
            "{size}: checksum untouched"
        );
        let node = InternalNode::new(&buf)?;
        ensure!(node.cell_count() == 0, "{size}: count");
        ensure!(
            node.rightmost_child()? == pid(0x0A0B_0C0D),
            "{size}: rightmost"
        );
        node.validate()?;
    }
    Ok(())
}

#[test]
fn init_rejects_page_zero_without_writing() -> anyhow::Result<()> {
    let mut buf = vec![0x5C; 512];
    ensure_error(
        InternalNodeMut::init(&mut buf, pid(0)),
        NodeError::CorruptNode {
            reason: "child page id is 0",
        },
        "init with page 0",
    )?;
    ensure!(buf.iter().all(|&b| b == 0x5C), "init changed the buffer");
    Ok(())
}

#[test]
fn new_requires_an_internal_page() -> anyhow::Result<()> {
    let mut slotted = vec![0u8; 512];
    SlottedPageMut::init(&mut slotted)?;
    let mut leaf = vec![0u8; 512];
    LeafNodeMut::init(&mut leaf)?;

    for (buf, found) in [(slotted, PageType::Slotted), (leaf, PageType::BTreeLeaf)] {
        let expected = NodeError::Page(PageError::UnexpectedPageType {
            expected: PageType::BTreeInternal,
            found,
        });
        ensure_error(InternalNode::new(&buf), expected, "read view")?;
        let mut copy = buf.clone();
        ensure_error(InternalNodeMut::new(&mut copy), expected, "mut view")?;
        ensure!(copy == buf, "new modified the page");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Byte layout
// ---------------------------------------------------------------------------

#[test]
fn cell_bytes() -> anyhow::Result<()> {
    let mut buf = node_page(512, 9)?;
    insert(&mut buf, 0, b"bb", 7)?;
    // child = 7, key "bb": 6 bytes at 506..512.
    ensure!(
        slice(&buf, 506, 6)? == [7, 0, 0, 0, b'b', b'b'],
        "cell bytes"
    );
    // Slot 0 = [offset 506 = 0x01FA, len 6].
    ensure!(slice(&buf, OFF_SLOTS, 4)? == [0xFA, 0x01, 6, 0], "slot 0");
    ensure!(u16_at(&buf, OFF_CELL_COUNT)? == 1, "cell_count");
    ensure!(u32_at(&buf, OFF_FREE_END)? == 506, "free_end");
    ensure!(slice(&buf, OFF_RIGHTMOST, 4)? == [9, 0, 0, 0], "rightmost");

    let node = InternalNode::new(&buf)?;
    ensure!(node.key(0)? == b"bb", "key");
    ensure!(node.child_at(0)? == pid(7), "child");
    node.validate()?;
    Ok(())
}

#[test]
fn cell_with_an_empty_key_is_four_bytes() -> anyhow::Result<()> {
    let mut buf = node_page(512, 9)?;
    insert(&mut buf, 0, b"", 0x0102_0304)?;
    ensure!(slice(&buf, 508, 4)? == [4, 3, 2, 1], "cell bytes");
    ensure!(InternalNode::new(&buf)?.key(0)?.is_empty(), "key");
    Ok(())
}

#[test]
fn views_borrow_from_the_page() -> anyhow::Result<()> {
    let buf = bdf_node()?;
    let range = buf.as_ptr_range();
    let node = InternalNode::new(&buf)?;
    ensure!(range.contains(&node.key(1)?.as_ptr()), "key");
    Ok(())
}

// ---------------------------------------------------------------------------
// child_for and child_at
// ---------------------------------------------------------------------------

#[test]
fn child_for_follows_invariant_4() -> anyhow::Result<()> {
    let buf = bdf_node()?;
    let node = InternalNode::new(&buf)?;
    // Keys b, d, f; children 1, 2, 3; rightmost 4.
    for (key, expected) in [
        (&b""[..], (0, pid(1))),
        (b"a", (0, pid(1))),
        (b"az", (0, pid(1))),
        // A key equal to a separator goes right.
        (b"b", (1, pid(2))),
        (b"c", (1, pid(2))),
        (b"d", (2, pid(3))),
        (b"e", (2, pid(3))),
        (b"f", (3, pid(4))),
        (b"g", (3, pid(4))),
        (b"zzz", (3, pid(4))),
    ] {
        ensure!(
            node.child_for(key)? == expected,
            "key {key:?}: got {:?}, expected {expected:?}",
            node.child_for(key)?
        );
    }
    Ok(())
}

#[test]
fn child_for_on_a_node_with_only_the_rightmost_child() -> anyhow::Result<()> {
    let buf = node_page(512, 11)?;
    let node = InternalNode::new(&buf)?;
    for key in [&b""[..], b"a", b"\xFF\xFF"] {
        ensure!(node.child_for(key)? == (0, pid(11)), "key {key:?}");
    }
    Ok(())
}

#[test]
fn child_for_with_an_empty_separator_key() -> anyhow::Result<()> {
    let mut buf = node_page(512, 4)?;
    insert(&mut buf, 0, b"", 1)?;
    let node = InternalNode::new(&buf)?;
    // Nothing is below the empty key, and the empty key goes right.
    ensure!(node.child_for(b"")? == (1, pid(4)), "empty key");
    ensure!(node.child_for(b"a")? == (1, pid(4)), "a");
    Ok(())
}

#[test]
fn search_returns_the_separator_position() -> anyhow::Result<()> {
    let buf = bdf_node()?;
    let node = InternalNode::new(&buf)?;
    ensure!(node.search(b"d")? == Ok(1), "match");
    ensure!(node.search(b"a")? == Err(0), "before all");
    ensure!(node.search(b"e")? == Err(2), "between");
    ensure!(node.search(b"z")? == Err(3), "after all");
    Ok(())
}

#[test]
fn child_at_covers_every_position() -> anyhow::Result<()> {
    let buf = bdf_node()?;
    let node = InternalNode::new(&buf)?;
    for (pos, expected) in [(0, 1), (1, 2), (2, 3), (3, 4)] {
        ensure!(node.child_at(pos)? == pid(expected), "pos {pos}");
    }
    ensure!(node.rightmost_child()? == pid(4), "rightmost");
    ensure_error(
        node.child_at(4),
        NodeError::Page(PageError::SlotOutOfRange {
            index: 4,
            cell_count: 3,
        }),
        "pos N + 1",
    )?;
    ensure!(
        matches!(
            node.child_at(usize::MAX),
            Err(NodeError::Page(PageError::SlotOutOfRange { .. }))
        ),
        "huge pos"
    );
    ensure!(
        matches!(
            node.key(3),
            Err(NodeError::Page(PageError::SlotOutOfRange { .. }))
        ),
        "key N"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing children
// ---------------------------------------------------------------------------

#[test]
fn set_child_at_writes_every_position() -> anyhow::Result<()> {
    for pos in 0..=3usize {
        let mut buf = bdf_node()?;
        let before = buf.clone();
        InternalNodeMut::new(&mut buf)?.set_child_at(pos, pid(0x00AB_CDEF))?;

        let node = InternalNode::new(&buf)?;
        for other in 0..=3 {
            let expected = if other == pos {
                pid(0x00AB_CDEF)
            } else {
                pid(u32::try_from(other)? + 1)
            };
            ensure!(node.child_at(other)? == expected, "pos {pos}, read {other}");
        }
        for key_pos in 0..3 {
            ensure!(
                node.key(key_pos)? == InternalNode::new(&before)?.key(key_pos)?,
                "pos {pos}: key {key_pos} changed"
            );
        }
        node.validate()?;
        // At most four bytes changed.
        let changed = buf.iter().zip(&before).filter(|(a, b)| a != b).count();
        ensure!(changed <= 4, "pos {pos}: {changed} bytes changed");
    }
    Ok(())
}

#[test]
fn set_child_at_n_writes_the_rightmost_pointer() -> anyhow::Result<()> {
    let mut buf = bdf_node()?;
    InternalNodeMut::new(&mut buf)?.set_child_at(3, pid(0x0102_0304))?;
    ensure!(slice(&buf, OFF_RIGHTMOST, 4)? == [4, 3, 2, 1], "raw bytes");
    ensure!(slice(&buf, OFF_RESERVED, 4)? == [0; 4], "reserved");
    ensure!(
        InternalNode::new(&buf)?.rightmost_child()? == pid(0x0102_0304),
        "read back"
    );

    InternalNodeMut::new(&mut buf)?.set_rightmost_child(pid(5))?;
    ensure!(
        slice(&buf, OFF_RIGHTMOST, 4)? == [5, 0, 0, 0],
        "set_rightmost"
    );
    Ok(())
}

#[test]
fn set_child_at_writes_the_first_four_bytes_of_the_cell() -> anyhow::Result<()> {
    let mut buf = node_page(512, 9)?;
    insert(&mut buf, 0, b"key", 1)?;
    InternalNodeMut::new(&mut buf)?.set_child_at(0, pid(0x0000_0102))?;
    // The cell is at 505..512: child, then "key".
    ensure!(
        slice(&buf, 505, 7)? == [2, 1, 0, 0, b'k', b'e', b'y'],
        "cell bytes"
    );
    Ok(())
}

#[test]
fn set_child_at_rejects_bad_arguments_without_writing() -> anyhow::Result<()> {
    let mut buf = bdf_node()?;
    let before = buf.clone();
    let zero = NodeError::CorruptNode {
        reason: "child page id is 0",
    };
    let mut node = InternalNodeMut::new(&mut buf)?;
    ensure_error(node.set_child_at(0, pid(0)), zero, "zero child in a cell")?;
    ensure_error(node.set_child_at(3, pid(0)), zero, "zero rightmost")?;
    ensure_error(
        node.set_rightmost_child(pid(0)),
        zero,
        "set_rightmost_child",
    )?;
    ensure_error(
        node.set_child_at(4, pid(1)),
        NodeError::Page(PageError::SlotOutOfRange {
            index: 4,
            cell_count: 3,
        }),
        "pos N + 1",
    )?;
    ensure!(buf == before, "a rejected write changed the page");
    Ok(())
}

// ---------------------------------------------------------------------------
// Insert, remove and limits
// ---------------------------------------------------------------------------

#[test]
fn insert_at_search_position_keeps_order() -> anyhow::Result<()> {
    let mut buf = node_page(512, 100)?;
    for (key, child) in [(&b"m"[..], 1u32), (b"c", 2), (b"x", 3), (b"f", 4)] {
        let Err(index) = InternalNode::new(&buf)?.search(key)? else {
            anyhow::bail!("duplicate");
        };
        insert(&mut buf, index, key, child)?;
    }
    let node = InternalNode::new(&buf)?;
    let keys: Vec<&[u8]> = (0..4).map(|i| node.key(i)).collect::<Result<_, _>>()?;
    ensure!(keys == [&b"c"[..], b"f", b"m", b"x"], "key order");
    for (pos, child) in [(0, 2), (1, 4), (2, 1), (3, 3), (4, 100)] {
        ensure!(node.child_at(pos)? == pid(child), "child {pos}");
    }
    node.validate()?;
    Ok(())
}

#[test]
fn key_of_max_len_works_and_one_more_byte_fails() -> anyhow::Result<()> {
    let mut buf = node_page(512, 9)?;
    let max_key = vec![b'k'; MAX_KEY_512];
    insert(&mut buf, 0, &max_key, 1)?;
    ensure!(InternalNode::new(&buf)?.key(0)? == max_key, "stored");

    let mut fresh = node_page(512, 9)?;
    let before = fresh.clone();
    ensure_error(
        insert(&mut fresh, 0, &[b'k'; MAX_KEY_512 + 1], 1),
        NodeError::KeyTooLarge {
            len: MAX_KEY_512 + 1,
            max: MAX_KEY_512,
        },
        "key + 1",
    )?;
    ensure!(fresh == before, "a rejected key changed the page");

    for (size, max_key) in [(4096usize, 1001usize), (65536, 16361)] {
        let mut buf = node_page(size, 9)?;
        insert(&mut buf, 0, &vec![b'k'; max_key], 1)?;
        ensure!(
            matches!(
                insert(&mut buf, 0, &vec![b'k'; max_key + 1], 1),
                Err(NodeError::KeyTooLarge { .. })
            ),
            "{size}: key + 1"
        );
    }
    Ok(())
}

#[test]
fn insert_rejects_page_zero_and_page_errors_without_writing() -> anyhow::Result<()> {
    let mut buf = bdf_node()?;
    let before = buf.clone();
    ensure_error(
        insert(&mut buf, 0, b"a", 0),
        NodeError::CorruptNode {
            reason: "child page id is 0",
        },
        "zero child",
    )?;
    ensure!(
        matches!(
            insert(&mut buf, 4, b"a", 1),
            Err(NodeError::Page(PageError::SlotOutOfRange { index: 4, .. }))
        ),
        "index past count"
    );
    ensure!(buf == before, "a rejected insert changed the page");

    // Fill the page: 48-byte keys give 52-byte cells and 56 bytes with the
    // slot, so 480 / 56 = 8 fit.
    let mut full = node_page(512, 9)?;
    for i in 0u8..8 {
        insert(&mut full, usize::from(i), &[i; 48], u32::from(i) + 1)?;
    }
    let before = full.clone();
    ensure!(
        matches!(
            insert(&mut full, 8, &[9; 48], 99),
            Err(NodeError::Page(PageError::PageFull { .. }))
        ),
        "page full"
    );
    ensure!(full == before, "PageFull changed the page");
    InternalNode::new(&full)?.validate()?;
    Ok(())
}

#[test]
fn remove_takes_out_the_key_and_its_child() -> anyhow::Result<()> {
    let mut buf = bdf_node()?;
    InternalNodeMut::new(&mut buf)?.remove(1)?;
    let node = InternalNode::new(&buf)?;
    ensure!(node.cell_count() == 2, "count");
    ensure!(node.key(0)? == b"b" && node.key(1)? == b"f", "keys");
    ensure!(node.child_at(0)? == pid(1), "child 0");
    ensure!(node.child_at(1)? == pid(3), "child 1");
    ensure!(node.child_at(2)? == pid(4), "rightmost");
    node.validate()?;

    ensure!(
        matches!(
            InternalNodeMut::new(&mut buf)?.remove(2),
            Err(NodeError::Page(PageError::SlotOutOfRange { index: 2, .. }))
        ),
        "remove past the end"
    );
    Ok(())
}

#[test]
fn slotted_operations_do_not_disturb_the_rightmost_child() -> anyhow::Result<()> {
    let mut buf = node_page(512, 42)?;
    for i in 0u8..8 {
        insert(&mut buf, usize::from(i), &[i; 48], u32::from(i) + 1)?;
    }
    let rightmost = |buf: &[u8]| -> anyhow::Result<()> {
        ensure!(slice(buf, OFF_RIGHTMOST, 4)? == [42, 0, 0, 0], "rightmost");
        ensure!(slice(buf, OFF_RESERVED, 4)? == [0; 4], "reserved");
        Ok(())
    };
    rightmost(&buf)?;
    InternalNodeMut::new(&mut buf)?.remove(2)?;
    rightmost(&buf)?;
    // This insert compacts first.
    insert(&mut buf, 2, &[2; 48], 3)?;
    rightmost(&buf)?;
    InternalNode::new(&buf)?.validate()?;
    Ok(())
}

#[test]
fn slotted_view_reports_free_space() -> anyhow::Result<()> {
    let mut buf = node_page(512, 9)?;
    ensure!(
        InternalNode::new(&buf)?.slotted().contiguous_free() == 480,
        "empty node"
    );
    insert(&mut buf, 0, b"key", 1)?;
    // 480 - (7 + 4).
    ensure!(
        InternalNode::new(&buf)?.slotted().total_free() == 469,
        "after one insert"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// validate
// ---------------------------------------------------------------------------

fn validate_fails(buf: &[u8], what: &str) -> anyhow::Result<()> {
    let outcome = InternalNode::new(buf).and_then(|node| node.validate());
    ensure!(outcome.is_err(), "{what}: validate passed");
    Ok(())
}

#[test]
fn validate_rejects_broken_nodes() -> anyhow::Result<()> {
    let base = bdf_node()?;

    // The three cells are at 497..512: b at 507, d at 502, f at 497.
    let mut zero_child = base.clone();
    put(&mut zero_child, 502, &[0, 0, 0, 0])?;
    validate_fails(&zero_child, "zero child in a cell")?;

    let mut zero_rightmost = base.clone();
    put(&mut zero_rightmost, OFF_RIGHTMOST, &[0, 0, 0, 0])?;
    validate_fails(&zero_rightmost, "zero rightmost child")?;

    let mut reserved = base.clone();
    put(&mut reserved, OFF_RESERVED, &[1, 0, 0, 0])?;
    validate_fails(&reserved, "non-zero reserved bytes")?;

    // The node does not check the order, so the caller can break it.
    let mut unordered = node_page(512, 9)?;
    insert(&mut unordered, 0, b"b", 1)?;
    insert(&mut unordered, 1, b"a", 2)?;
    validate_fails(&unordered, "descending keys")?;

    let mut duplicate = node_page(512, 9)?;
    insert(&mut duplicate, 0, b"a", 1)?;
    insert(&mut duplicate, 1, b"a", 2)?;
    validate_fails(&duplicate, "duplicate keys")?;

    let mut slots = base;
    put(&mut slots, OFF_SLOTS, &[0, 0, 0, 0])?;
    validate_fails(&slots, "corrupt slot")
}

#[test]
fn reading_a_zero_child_is_corrupt_node() -> anyhow::Result<()> {
    let mut buf = bdf_node()?;
    put(&mut buf, 502, &[0, 0, 0, 0])?;
    let node = InternalNode::new(&buf)?;
    ensure!(
        matches!(node.child_at(1), Err(NodeError::CorruptNode { .. })),
        "child_at"
    );
    ensure!(
        matches!(node.child_for(b"c"), Err(NodeError::CorruptNode { .. })),
        "child_for"
    );
    // Other children still read.
    ensure!(node.child_at(0)? == pid(1), "other child");

    put(&mut buf, OFF_RIGHTMOST, &[0, 0, 0, 0])?;
    ensure!(
        matches!(
            InternalNode::new(&buf)?.rightmost_child(),
            Err(NodeError::CorruptNode { .. })
        ),
        "rightmost"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Property test (page size 512)
// ---------------------------------------------------------------------------

/// Inserts every distinct key at its search position until the page is full
/// and checks the node against a sorted model, including `child_for`.
fn run_model(keys: Vec<Vec<u8>>, probes: &[Vec<u8>]) -> anyhow::Result<()> {
    let mut buf = node_page(512, 1000)?;
    let mut model: Vec<(Vec<u8>, u32)> = Vec::new();
    let mut next_child = 1u32;

    for key in keys {
        let Err(index) = InternalNode::new(&buf)?.search(&key)? else {
            ensure!(
                model.iter().any(|(k, _)| *k == key),
                "search found a missing key"
            );
            continue;
        };
        match insert(&mut buf, index, &key, next_child) {
            Ok(()) => {
                model.insert(index, (key, next_child));
                next_child = next_child.checked_add(1).context("child overflow")?;
            }
            Err(NodeError::Page(PageError::PageFull { .. })) => break,
            Err(error) => return Err(error.into()),
        }
        InternalNode::new(&buf)?.validate()?;
    }

    let node = InternalNode::new(&buf)?;
    ensure!(node.cell_count() == model.len(), "cell count");
    for (index, (key, child)) in model.iter().enumerate() {
        ensure!(node.key(index)? == key.as_slice(), "key at {index}");
        ensure!(node.child_at(index)? == pid(*child), "child at {index}");
    }
    ensure!(
        node.child_at(model.len())? == pid(1000),
        "rightmost after the inserts"
    );

    // child_for: the first position whose key is greater than the probe.
    let all_probes = probes.iter().chain(model.iter().map(|(k, _)| k));
    for probe in all_probes {
        let pos = model.partition_point(|(k, _)| k <= probe);
        let expected = model.get(pos).map_or(1000, |(_, child)| *child);
        ensure!(
            node.child_for(probe)? == (pos, pid(expected)),
            "child_for {probe:?}"
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_inserts_at_search_position_match_a_sorted_model(
        keys in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..=12), 1..40),
        probes in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..=12), 0..10),
    ) {
        run_model(keys, &probes).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
