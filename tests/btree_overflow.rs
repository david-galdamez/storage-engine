//! Tests for overflow values (Phase 2, step 8).
//!
//! Page size is 512 bytes, so an overflow page holds 488 bytes and the largest
//! inline value for the key `b"key"` is 110 bytes. The tests check:
//!
//! - the overflow page views: round trip, `data_len` past the capacity, wrong
//!   page type, non-zero tail, and `init` with too much data;
//! - put and get at the size boundaries (the inline maximum and one more, one
//!   page, `k` pages and `k` pages plus or minus one byte, 1 MiB), with the
//!   number of chain pages equal to `ceil(len / capacity)`;
//! - replacing inline with overflow, overflow with inline and overflow with
//!   overflow: the old chain goes to the free list and its pages are reused;
//! - many overflow cells in a tree that splits at the leaf and internal
//!   levels, read back after reopening the file;
//! - corrupt chains built by hand (too short, too long, a cycle, a page of the
//!   wrong type, a non-last page that is not full, a non-zero tail), reported
//!   as `CorruptTree` or `Node` from `get` and from the free path of `put`;
//! - a property test against a `BTreeMap` model with small and large values.
//!
//! After every tree is built, `check_tree_pages` validates the tree and its
//! chains, and `ensure_no_leaks` checks that every page of the file is a node,
//! an overflow page or a free page. The `ValueTooLarge` check needs a value of
//! more than 4 GiB, so it is unit tested on its helper in `tree.rs` and not
//! here.

mod btree_check;
#[path = "support/build.rs"]
mod build;
#[path = "support/db.rs"]
mod db;
#[path = "support/leaks.rs"]
mod leaks;
#[path = "support/lookup.rs"]
mod lookup;
#[path = "support/shape.rs"]
mod shape;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use btree_check::{Entries, check_tree, leaf_chain};
use build::{bulk_load, write_leaf};
use db::{PAGE_SIZE, new_pager, page_size, temp_db};
use leaks::ensure_no_leaks;
use lookup::{ensure_missing, ensure_value, get};
use proptest::prelude::*;
use shape::{blank, height, numbered_entries};
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::btree::error::{BTreeError, NodeError};
use storage_engine_rs::storage::btree::leaf::{LeafNode, LeafNodeMut};
use storage_engine_rs::storage::btree::node::{ValueRef, max_node_cell_len};
use storage_engine_rs::storage::btree::overflow::{
    OVERFLOW_HEADER_SIZE, OverflowPage, OverflowPageMut, overflow_capacity, read_value,
};
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::error::PageError;
use storage_engine_rs::storage::pager::types::PageId;

const KEY: &[u8] = b"key";

fn capacity() -> anyhow::Result<usize> {
    Ok(overflow_capacity(page_size()?))
}

/// The largest inline value next to `KEY`.
fn max_inline() -> anyhow::Result<usize> {
    max_node_cell_len(page_size()?)
        .checked_sub(3)
        .and_then(|n| n.checked_sub(KEY.len()))
        .context("limit underflow")
}

/// `len` bytes that depend on the position and on `seed`, so a chain with
/// pages in the wrong order or a wrong chunk size does not match.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| {
            let mixed = i.wrapping_mul(31).wrapping_add(usize::from(seed)) % 251;
            u8::try_from(mixed).unwrap_or(0)
        })
        .collect()
}

fn key_of(i: usize) -> Vec<u8> {
    format!("key-{i:05}").into_bytes()
}

const fn pages_for(len: usize, capacity: usize) -> usize {
    len.div_ceil(capacity)
}

fn read(pager: &Pager, id: PageId) -> anyhow::Result<Vec<u8>> {
    let mut buf = blank();
    pager.read_page(id, &mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------- views

#[test]
fn view_round_trip() -> anyhow::Result<()> {
    let capacity = capacity()?;
    ensure!(capacity == PAGE_SIZE - OVERFLOW_HEADER_SIZE, "capacity");
    for (len, next) in [
        (0, None),
        (1, None),
        (100, Some(PageId::new(7))),
        (capacity, Some(PageId::new(9))),
    ] {
        let data = pattern(len, 3);
        let mut buf = vec![0xAA; PAGE_SIZE];
        OverflowPageMut::init(&mut buf, next, &data)?;
        let page = OverflowPage::new(&buf)?;
        page.validate()?;
        ensure!(page.data()? == data.as_slice(), "data, len {len}");
        ensure!(page.next()? == next, "next, len {len}");
    }
    Ok(())
}

#[test]
fn view_rejects_data_len_past_the_capacity() -> anyhow::Result<()> {
    let mut buf = blank();
    OverflowPageMut::init(&mut buf, None, b"abc")?;
    let too_long = u32::try_from(capacity()?.checked_add(1).context("overflow")?)?;
    buf.get_mut(20..24)
        .context("data_len bytes")?
        .copy_from_slice(&too_long.to_le_bytes());
    let page = OverflowPage::new(&buf)?;
    ensure!(
        matches!(page.data(), Err(NodeError::CorruptNode { .. })),
        "data"
    );
    ensure!(
        matches!(page.validate(), Err(NodeError::CorruptNode { .. })),
        "validate"
    );
    Ok(())
}

#[test]
fn view_rejects_a_wrong_page_type() -> anyhow::Result<()> {
    let mut buf = blank();
    LeafNodeMut::init(&mut buf)?;
    match OverflowPage::new(&buf) {
        Err(NodeError::Page(PageError::UnexpectedPageType { .. })) => Ok(()),
        other => bail!("expected UnexpectedPageType, got {other:?}"),
    }
}

#[test]
fn view_rejects_a_non_zero_tail() -> anyhow::Result<()> {
    let mut buf = blank();
    OverflowPageMut::init(&mut buf, None, b"abc")?;
    *buf.last_mut().context("empty buffer")? = 1;
    let page = OverflowPage::new(&buf)?;
    ensure!(
        matches!(page.validate(), Err(NodeError::CorruptNode { .. })),
        "validate"
    );
    Ok(())
}

#[test]
fn init_rejects_too_much_data() -> anyhow::Result<()> {
    let mut buf = vec![0xAA; PAGE_SIZE];
    let data = vec![1u8; capacity()?.checked_add(1).context("overflow")?];
    match OverflowPageMut::init(&mut buf, None, &data) {
        Err(NodeError::CellTooLarge { len, max }) => {
            ensure!(
                len == data.len() && max == capacity()?,
                "len {len} max {max}"
            );
        }
        other => bail!("expected CellTooLarge, got {other:?}"),
    }
    ensure!(buf.iter().all(|&b| b == 0xAA), "buffer was modified");
    Ok(())
}

// ------------------------------------------------------------ boundaries

fn check_size(len: usize) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let value = pattern(len, 5);

    ensure!(!tree.put(&mut pager, KEY, &value)?, "len {len}: new key");
    ensure_value(get(tree, &pager, KEY)?.as_deref(), &value, "round trip")?;
    let (overflow, entries) = ensure_no_leaks(&pager, tree)?;
    let expected = if len <= max_inline()? {
        0
    } else {
        pages_for(len, capacity()?)
    };
    ensure!(
        overflow == expected,
        "len {len}: {overflow} pages, expected {expected}"
    );
    ensure!(entries == vec![(KEY.to_vec(), value)], "len {len}: entries");
    Ok(())
}

#[test]
fn sizes_around_every_boundary() -> anyhow::Result<()> {
    let capacity = capacity()?;
    let inline = max_inline()?;
    let mut sizes = vec![inline, inline + 1, capacity - 1, capacity, capacity + 1];
    for k in [2usize, 3, 5, 8] {
        let n = k * capacity;
        sizes.extend([n - 1, n, n + 1]);
    }
    for len in sizes {
        check_size(len)?;
    }
    Ok(())
}

#[test]
fn one_mebibyte_value() -> anyhow::Result<()> {
    check_size(1024 * 1024)
}

#[test]
fn read_value_appends_to_out() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let value = pattern(1500, 1);
    tree.put(&mut pager, KEY, &value)?;
    let buf = read(&pager, tree.root())?;
    let cell = LeafNode::new(&buf)?.cell(0)?;

    let mut out = b"head".to_vec();
    read_value(&pager, cell.value, &mut out)?;
    ensure!(out.get(..4) == Some(b"head".as_slice()), "head kept");
    ensure!(out.get(4..) == Some(value.as_slice()), "value appended");
    read_value(&pager, ValueRef::Inline(b"!"), &mut out)?;
    ensure!(out.last() == Some(&b'!'), "inline appended");
    Ok(())
}

// --------------------------------------------------------------- replace

fn free_count(pager: &Pager) -> anyhow::Result<usize> {
    Ok(usize::try_from(pager.free_page_count())?)
}

#[test]
fn replace_inline_with_overflow_and_back() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let capacity = capacity()?;
    let big = pattern(2 * capacity + 10, 2);

    tree.put(&mut pager, KEY, b"small")?;
    ensure!(tree.put(&mut pager, KEY, &big)?, "inline to overflow");
    ensure!(free_count(&pager)? == 0, "nothing to free yet");
    ensure!(ensure_no_leaks(&pager, tree)?.0 == 3, "three pages");
    ensure_value(get(tree, &pager, KEY)?.as_deref(), &big, "big")?;

    ensure!(tree.put(&mut pager, KEY, b"small")?, "overflow to inline");
    ensure!(free_count(&pager)? == 3, "old chain freed");
    ensure!(ensure_no_leaks(&pager, tree)?.0 == 0, "no chain left");
    ensure_value(get(tree, &pager, KEY)?.as_deref(), b"small", "small")?;
    Ok(())
}

#[test]
fn replace_overflow_with_overflow_reuses_freed_pages() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let capacity = capacity()?;

    tree.put(&mut pager, KEY, &pattern(3 * capacity, 1))?;
    let after_first = pager.page_count();
    // The new chain is written before the old one is freed, so the first
    // replace grows the file by one chain...
    ensure!(
        tree.put(&mut pager, KEY, &pattern(3 * capacity, 2))?,
        "replace 1"
    );
    ensure!(free_count(&pager)? == 3, "old chain freed");
    let after_second = pager.page_count();
    ensure!(
        after_second == after_first + 3,
        "first replace grows by a chain"
    );
    ensure_no_leaks(&pager, tree)?;
    // ...and the next replaces reuse the freed pages.
    for seed in 3..6u8 {
        let value = pattern(3 * capacity, seed);
        ensure!(tree.put(&mut pager, KEY, &value)?, "replace {seed}");
        ensure!(
            pager.page_count() == after_second,
            "file grew on replace {seed}"
        );
        ensure!(free_count(&pager)? == 3, "free count on replace {seed}");
        ensure_value(get(tree, &pager, KEY)?.as_deref(), &value, "value")?;
        ensure_no_leaks(&pager, tree)?;
    }

    // A shorter chain leaves more pages free; a longer one takes them back.
    tree.put(&mut pager, KEY, &pattern(capacity + 1, 7))?;
    ensure!(
        free_count(&pager)? == 4,
        "2 pages used, 3 freed, 1 taken: 4 free"
    );
    tree.put(&mut pager, KEY, &pattern(5 * capacity, 8))?;
    ensure_value(
        get(tree, &pager, KEY)?.as_deref(),
        &pattern(5 * capacity, 8),
        "longer value",
    )?;
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

#[test]
fn replace_in_a_full_leaf_frees_the_old_chain_after_the_split() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let capacity = capacity()?;

    // Fill the root leaf with inline cells and one overflow cell, then replace
    // the overflow cell with a value that is overflow again: the leaf is full.
    for i in 0..4usize {
        tree.put(&mut pager, &key_of(i), &pattern(80, 1))?;
    }
    tree.put(&mut pager, &key_of(2), &pattern(2 * capacity, 1))?;
    for round in 0..4u8 {
        let value = pattern(2 * capacity + usize::from(round), round);
        ensure!(tree.put(&mut pager, &key_of(2), &value)?, "replace");
        ensure_value(get(tree, &pager, &key_of(2))?.as_deref(), &value, "value")?;
        ensure_no_leaks(&pager, tree)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- splits

fn reopen_and_check(
    path: &Path,
    root: PageId,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> anyhow::Result<()> {
    let pager = Pager::open(path)?;
    let tree = BTree::open(&pager, root)?;
    for (key, value) in model {
        ensure_value(get(tree, &pager, key)?.as_deref(), value, "after reopen")?;
    }
    let (_, entries) = ensure_no_leaks(&pager, tree)?;
    ensure!(entries.len() == model.len(), "entry count after reopen");
    Ok(())
}

#[test]
fn many_overflow_cells_split_every_level_and_survive_reopen() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let root = tree.root();
    let mut model = BTreeMap::new();

    // 401 is prime, so `i * 131 % 401` visits every key once in a scrambled
    // order and the splits are not only right-only appends.
    for i in 0..401usize {
        let n = i * 131 % 401;
        let value = pattern(120 + (n * 37) % 900, u8::try_from(n % 200)?);
        ensure!(
            !tree.put(&mut pager, &key_of(n), &value)?,
            "key {n} existed"
        );
        model.insert(key_of(n), value);
    }
    ensure!(
        height(&pager, root)? >= 3,
        "height {}",
        height(&pager, root)?
    );
    ensure!(tree.root() == root, "root page id changed");
    let (overflow, entries) = ensure_no_leaks(&pager, tree)?;
    ensure!(overflow > 401, "overflow pages: {overflow}");
    ensure!(
        entries == model.clone().into_iter().collect::<Vec<_>>(),
        "entries differ from the model"
    );
    drop(pager);
    reopen_and_check(&path, root, &model)
}

#[test]
fn sequential_overflow_keys_use_right_only_appends() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let mut model = BTreeMap::new();
    for n in 0..150usize {
        let value = pattern(200 + n * 7, u8::try_from(n)?);
        tree.put(&mut pager, &key_of(n), &value)?;
        model.insert(key_of(n), value);
    }
    ensure_no_leaks(&pager, tree)?;
    for (key, value) in &model {
        ensure_value(get(tree, &pager, key)?.as_deref(), value, "sequential")?;
    }
    Ok(())
}

#[test]
fn overflow_values_in_a_hand_built_three_level_tree() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let entries = numbered_entries(400);
    let root = bulk_load(&mut pager, &entries)?;
    ensure!(height(&pager, root)? >= 3, "needs three levels");
    let tree = BTree::open(&pager, root)?;
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();

    // Replace every 5th key with an overflow value, and add new keys (odd
    // numbers are absent from `numbered_entries`) between the existing ones.
    for (n, key) in model
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .enumerate()
    {
        if n % 5 == 0 {
            let value = pattern(150 + n, u8::try_from(n % 200)?);
            ensure!(tree.put(&mut pager, &key, &value)?, "replace {n}");
            model.insert(key, value);
        }
    }
    for n in (1..400usize).step_by(40) {
        let key = format!("key-{:05}", n * 2 + 1).into_bytes();
        let value = pattern(600, 4);
        ensure!(!tree.put(&mut pager, &key, &value)?, "insert {n}");
        model.insert(key, value);
    }

    ensure!(tree.root() == root, "root page id changed");
    ensure_no_leaks(&pager, tree)?;
    let expected: Entries = model.into_iter().collect();
    ensure!(check_tree(&pager, tree)? == expected, "entries differ");
    ensure!(leaf_chain(&pager, tree)?.len() > 1, "leaf chain");
    Ok(())
}

#[test]
fn get_reads_a_hand_built_overflow_cell() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let id = write_leaf(&mut pager, &[(b"a", b"A")], None)?;
    // A chain of two pages, written backwards by hand.
    let second = pager.allocate_page()?;
    let first = pager.allocate_page()?;
    let capacity = capacity()?;
    let value = pattern(capacity + 30, 6);
    let (head, tail) = value.split_at(capacity);
    let mut buf = blank();
    OverflowPageMut::init(&mut buf, None, tail)?;
    pager.write_page(second, &mut buf)?;
    OverflowPageMut::init(&mut buf, Some(second), head)?;
    pager.write_page(first, &mut buf)?;
    let mut leaf = read(&pager, id)?;
    LeafNodeMut::new(&mut leaf)?.insert(
        1,
        b"big",
        ValueRef::Overflow {
            len: u32::try_from(value.len())?,
            first_page: first,
        },
    )?;
    pager.write_page(id, &mut leaf)?;
    let tree = BTree::open(&pager, id)?;

    ensure_value(get(tree, &pager, b"big")?.as_deref(), &value, "big")?;
    ensure_value(get(tree, &pager, b"a")?.as_deref(), b"A", "a")
}

// ----------------------------------------------------- corrupt chains

/// A tree with one key whose value takes 3 pages (2 full ones and `extra`
/// bytes), on a fresh file: the chain is pages 2, 3 and 4.
struct Chain3 {
    _dir: tempfile::TempDir,
    pager: Pager,
    tree: BTree,
}

impl Chain3 {
    /// A chain whose last page has 100 bytes.
    fn new() -> anyhow::Result<Self> {
        Self::with_extra(100)
    }

    fn with_extra(extra: usize) -> anyhow::Result<Self> {
        let (dir, path) = temp_db()?;
        let mut pager = new_pager(&path)?;
        let tree = BTree::create(&mut pager)?;
        let len = capacity()?
            .checked_mul(2)
            .and_then(|n| n.checked_add(extra))
            .context("overflow")?;
        tree.put(&mut pager, KEY, &pattern(len, 9))?;
        let buf = read(&pager, tree.root())?;
        let first = match LeafNode::new(&buf)?.cell(0)?.value {
            ValueRef::Overflow { first_page, .. } => first_page,
            ValueRef::Inline(_) => bail!("expected an overflow cell"),
        };
        ensure!(first == PageId::new(2), "chain starts at {first}");
        Ok(Self {
            _dir: dir,
            pager,
            tree,
        })
    }

    /// Rewrites chain page `id` with `data_len` bytes and the link `next`.
    fn rewrite(&mut self, id: u32, next: Option<u32>, data_len: usize) -> anyhow::Result<()> {
        let mut buf = blank();
        OverflowPageMut::init(&mut buf, next.map(PageId::new), &pattern(data_len, 1))?;
        self.pager.write_page(PageId::new(id), &mut buf)?;
        Ok(())
    }

    /// `get` and then `put` (which frees the old chain) must both fail with
    /// `CorruptTree` at page `page` for `reason`.
    fn expect_corrupt(&mut self, page: u32, reason: &str) -> anyhow::Result<()> {
        self.expect_corrupt_split(page, reason, page, reason)
    }

    /// Like `expect_corrupt`, with a different expected error for `put`: it
    /// frees the pages it already walked, which can change what it finds.
    fn expect_corrupt_split(
        &mut self,
        page: u32,
        reason: &str,
        put_page: u32,
        put_reason: &str,
    ) -> anyhow::Result<()> {
        let mut out = Vec::new();
        let err = match self.tree.get(&self.pager, KEY, &mut out) {
            Ok(found) => bail!("get should fail, returned {found}"),
            Err(e) => e,
        };
        ensure_corrupt(&err, page, reason).context("get")?;
        ensure!(out.is_empty(), "out should be empty after a failed read");

        let err = match self.tree.put(&mut self.pager, KEY, b"small") {
            Ok(replaced) => bail!("put should fail, returned {replaced}"),
            Err(e) => e,
        };
        ensure_corrupt(&err, put_page, put_reason).context("put")
    }
}

fn ensure_corrupt(err: &BTreeError, page: u32, reason: &str) -> anyhow::Result<()> {
    match err {
        BTreeError::CorruptTree { page: p, reason: r } => {
            ensure!(p.get() == page, "wrong page: {p}");
            ensure!(*r == reason, "wrong reason: {r}");
            Ok(())
        }
        other => bail!("expected CorruptTree, got {other:?}"),
    }
}

#[test]
fn chain_that_ends_early() -> anyhow::Result<()> {
    let mut c = Chain3::new()?;
    // Page 3 is full but ends the chain: 976 bytes of 1076.
    c.rewrite(3, None, capacity()?)?;
    c.expect_corrupt(3, "overflow chain ends before the value length")
}

#[test]
fn chain_that_continues_past_the_length() -> anyhow::Result<()> {
    // The value is exactly three full pages, and the last one links on.
    let mut c = Chain3::with_extra(capacity()?)?;
    c.rewrite(4, Some(5), capacity()?)?;
    c.expect_corrupt(4, "overflow chain continues past the value length")
}

#[test]
fn chain_that_is_a_cycle() -> anyhow::Result<()> {
    let mut c = Chain3::new()?;
    // 2 -> 3 -> 2: the third read has more data than the 100 bytes left.
    c.rewrite(3, Some(2), capacity()?)?;
    // `put` frees page 2 on its first step, so it meets the cycle as a free
    // page instead.
    c.expect_corrupt_split(
        2,
        "overflow chain is longer than the value length",
        2,
        "overflow chain page is not an overflow page",
    )
}

#[test]
fn chain_page_of_the_wrong_type() -> anyhow::Result<()> {
    let mut c = Chain3::new()?;
    let mut buf = blank();
    LeafNodeMut::init(&mut buf)?;
    c.pager.write_page(PageId::new(3), &mut buf)?;
    c.expect_corrupt(3, "overflow chain page is not an overflow page")
}

#[test]
fn non_last_page_that_is_not_full() -> anyhow::Result<()> {
    let mut c = Chain3::new()?;
    c.rewrite(2, Some(3), 100)?;
    c.expect_corrupt(2, "a non-last overflow page is not full")
}

#[test]
fn chain_page_with_a_non_zero_tail_is_a_node_error() -> anyhow::Result<()> {
    let mut c = Chain3::new()?;
    let mut buf = read(&c.pager, PageId::new(4))?;
    *buf.last_mut().context("empty buffer")? = 1;
    c.pager.write_page(PageId::new(4), &mut buf)?;

    let mut out = Vec::new();
    match c.tree.get(&c.pager, KEY, &mut out) {
        Err(BTreeError::Node {
            page,
            source: NodeError::CorruptNode { .. },
        }) => {
            ensure!(page == PageId::new(4), "page {page}");
        }
        other => bail!("expected a node error, got {other:?}"),
    }
    Ok(())
}

#[test]
fn a_miss_does_not_read_the_chain() -> anyhow::Result<()> {
    let mut c = Chain3::new()?;
    c.rewrite(2, None, 1)?;
    ensure_missing(get(c.tree, &c.pager, b"other")?.as_deref(), "other key")
}

// -------------------------------------------------------------- proptest

fn run_model(keys: &[Vec<u8>], ops: &[(usize, usize, u8)]) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    for &(key_index, len, seed) in ops {
        let key = keys
            .get(key_index.checked_rem(keys.len()).context("no keys")?)
            .context("key")?;
        let value = pattern(len, seed);
        let replaced = tree.put(&mut pager, key, &value)?;
        ensure!(
            replaced == model.insert(key.clone(), value).is_some(),
            "replaced flag"
        );
    }

    let (_, entries) = ensure_no_leaks(&pager, tree)?;
    ensure!(
        entries
            == model
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Vec<_>>(),
        "entries differ from the model"
    );
    for (key, value) in &model {
        ensure_value(get(tree, &pager, key)?.as_deref(), value, "model value")?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn prop_mixed_values_match_the_model(
        keys in prop::collection::vec(
            prop::collection::vec(prop::sample::select(vec![b'a', b'b', b'c']), 0..10),
            1..=10,
        ),
        ops in prop::collection::vec(
            (
                any::<usize>(),
                prop_oneof![0usize..120, 100usize..1200, 480usize..500, 960usize..980],
                any::<u8>(),
            ),
            1..50,
        ),
    ) {
        run_model(&keys, &ops).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
