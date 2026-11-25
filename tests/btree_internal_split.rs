//! Tests for internal node splits in `BTree::put` (Phase 2, step 7). Page size
//! is 512 bytes, so trees grow tall with a few hundred entries.
//!
//! Keys of 100 bytes (the limit is 105) give leaves and internal nodes with
//! only 4 entries, so height 4 and 5 appear after a few hundred inserts.
//! Short keys (7 bytes, 20-byte values) give wide nodes.
//!
//! The tests check:
//!
//! - growth to height 3, 4 and 5 with ascending, descending and scrambled
//!   inserts: after every few inserts `check_tree` passes (bounds, equal leaf
//!   depth, leaf chain), the leaf chain visits every key in order, `get` finds
//!   every key and the root page id never changes;
//! - right-only appends in internal nodes: with ascending inserts every
//!   internal node except the last one of its level is almost full, and the
//!   file is much smaller than a split by halves would give;
//! - a hand-built deep tree (`bulk_load`) grows two more levels with inserts
//!   between its keys, and a hand-built root leaf grows deep;
//! - a deep tree survives closing and reopening the file;
//! - a property test against a `BTreeMap` model with keys that force height 3.
//!
//! A corrupt-parent case for an internal level is not tested: the crumbs come
//! from the same read of the page that the split checks, so the child never
//! differs unless the page changes between two reads of one `put`, which
//! cannot be built from a file.

mod btree_check;
#[path = "support/build.rs"]
mod build;
#[path = "support/db.rs"]
mod db;
#[path = "support/lookup.rs"]
mod lookup;
#[path = "support/shape.rs"]
mod shape;

use std::collections::BTreeMap;

use anyhow::{Context, ensure};
use btree_check::{Entries, check_tree, leaf_chain};
use build::{bulk_load, write_leaf};
use db::{PAGE_SIZE, new_pager, temp_db};
use lookup::{ensure_missing, ensure_value, get};
use proptest::prelude::*;
use shape::{blank, height, numbered_entries};
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::btree::internal::InternalNode;
use storage_engine_rs::storage::btree::leaf::LeafNode;
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::types::PageId;

/// Usable bytes of a node page: the page minus the 32-byte header.
const USABLE: usize = PAGE_SIZE - 32;

/// Bytes of one internal cell with a short key (4 + 7) and its slot (4).
const SHORT_CELL: usize = 15;

fn short_key(i: usize) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

/// A 100-byte key, so a node holds only 4 entries.
fn long_key(i: usize) -> Vec<u8> {
    format!("{i:0>100}").into_bytes()
}

fn read_page(pager: &Pager, id: PageId) -> anyhow::Result<Vec<u8>> {
    let mut buf = blank();
    pager.read_page(id, &mut buf)?;
    Ok(buf)
}

/// A scrambled order of `0..count` for a prime `count` (37 is coprime with it).
fn scrambled(count: usize) -> Vec<usize> {
    (0..count)
        .map(|i| i.saturating_mul(37).checked_rem(count).unwrap_or(0))
        .collect()
}

/// The page ids of every level, from the root down to the leaves, in order.
fn levels(pager: &Pager, root: PageId) -> anyhow::Result<Vec<Vec<PageId>>> {
    let mut all = Vec::new();
    let mut current = vec![root];
    loop {
        let mut next = Vec::new();
        for &id in &current {
            let buf = read_page(pager, id)?;
            if let Ok(node) = InternalNode::new(&buf) {
                for pos in 0..=node.cell_count() {
                    next.push(node.child_at(pos)?);
                }
            }
        }
        all.push(current);
        if next.is_empty() {
            return Ok(all);
        }
        current = next;
    }
}

/// Number of internal nodes in the tree.
fn internal_count(pager: &Pager, root: PageId) -> anyhow::Result<usize> {
    let all = levels(pager, root)?;
    Ok(all
        .iter()
        .take(all.len().saturating_sub(1))
        .map(Vec::len)
        .sum())
}

/// Every key found by following the leaf chain from the leftmost leaf.
fn chain_keys(pager: &Pager, tree: BTree) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut keys = Vec::new();
    for id in leaf_chain(pager, tree)? {
        let buf = read_page(pager, id)?;
        let leaf = LeafNode::new(&buf)?;
        for index in 0..leaf.cell_count() {
            keys.push(leaf.key(index)?.to_vec());
        }
    }
    Ok(keys)
}

/// Checks everything that a split can break against the expected `model`:
/// the tree invariants, the leaf chain, the root id and, if `full`, a `get`
/// of every key.
fn check_against(
    pager: &Pager,
    tree: BTree,
    root: PageId,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    full: bool,
) -> anyhow::Result<()> {
    ensure!(tree.root() == root, "the root page id changed");
    let entries = check_tree(pager, tree)?;
    let expected: Entries = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    ensure!(entries == expected, "entries differ from the model");
    let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
    ensure!(chain_keys(pager, tree)? == keys, "leaf chain differs");
    if full {
        for (k, v) in model {
            ensure_value(get(tree, pager, k)?.as_deref(), v, "get")?;
        }
    }
    Ok(())
}

/// The tallest height reached and the model of a `grow` run.
type Grown = (usize, BTreeMap<Vec<u8>, Vec<u8>>);

/// Puts `key(i)` for every `i` in `order`, checking the tree every `step`
/// inserts. Returns the largest height seen and the final model.
fn grow(
    pager: &mut Pager,
    tree: BTree,
    order: &[usize],
    key: fn(usize) -> Vec<u8>,
    step: usize,
) -> anyhow::Result<Grown> {
    let root = tree.root();
    let mut model = BTreeMap::new();
    let mut tallest = 1usize;
    for (n, &i) in order.iter().enumerate() {
        let value = i.to_le_bytes().to_vec();
        ensure!(
            !tree.put(pager, &key(i), &value)?,
            "key {i} reported a replace"
        );
        model.insert(key(i), value);
        tallest = tallest.max(height(pager, root)?);
        if n.checked_rem(step) == Some(0) {
            check_against(pager, tree, root, &model, false)?;
        }
    }
    check_against(pager, tree, root, &model, true)?;
    Ok((tallest, model))
}

#[test]
fn ascending_inserts_grow_to_height_five() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;

    let order: Vec<usize> = (0..700).collect();
    let (tallest, _) = grow(&mut pager, tree, &order, long_key, 10)?;
    ensure!(tallest >= 5, "only reached height {tallest}");
    Ok(())
}

#[test]
fn descending_inserts_grow_to_height_four() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;

    let order: Vec<usize> = (0..600).rev().collect();
    let (tallest, _) = grow(&mut pager, tree, &order, long_key, 10)?;
    ensure!(tallest >= 4, "only reached height {tallest}");
    Ok(())
}

#[test]
fn scrambled_inserts_grow_to_height_four() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;

    let (tallest, _) = grow(&mut pager, tree, &scrambled(641), long_key, 10)?;
    ensure!(tallest >= 4, "only reached height {tallest}");
    Ok(())
}

#[test]
fn short_keys_reach_height_three_in_every_order() -> anyhow::Result<()> {
    for order in [
        (0..2500).collect::<Vec<_>>(),
        (0..2500).rev().collect(),
        scrambled(2503),
    ] {
        let (_dir, path) = temp_db()?;
        let mut pager = new_pager(&path)?;
        let tree = BTree::create(&mut pager)?;
        let (tallest, _) = grow(&mut pager, tree, &order, short_key, 50)?;
        ensure!(tallest >= 3, "only reached height {tallest}");
    }
    Ok(())
}

#[test]
fn root_split_keeps_the_root_page_and_adds_one_level() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let root = tree.root();

    let mut seen = 1usize;
    for i in 0..700usize {
        tree.put(&mut pager, &long_key(i), b"v")?;
        let now = height(&pager, root)?;
        ensure!(now <= seen.saturating_add(1), "height jumped to {now}");
        if now > seen {
            // The root is an internal node with one separator.
            let buf = read_page(&pager, root)?;
            let node = InternalNode::new(&buf)?;
            ensure!(node.cell_count() == 1, "new root has {}", node.cell_count());
            seen = now;
        }
    }
    ensure!(seen >= 5, "only reached height {seen}");
    Ok(())
}

#[test]
fn ascending_inserts_leave_internal_nodes_almost_full() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let root = tree.root();

    let order: Vec<usize> = (0..3000).collect();
    grow(&mut pager, tree, &order, short_key, 500)?;

    let all = levels(&pager, root)?;
    ensure!(all.len() >= 3, "only {} levels", all.len());
    // Skip the leaf level. On each internal level, every node but the last
    // lost only its final cell in a right-only append, so it has room for
    // fewer than two more cells. A split by halves would leave half free.
    let internal_levels = all.len().saturating_sub(1);
    for (depth, ids) in all.iter().take(internal_levels).enumerate() {
        let (_, others) = ids.split_last().context("empty level")?;
        for &id in others {
            let buf = read_page(&pager, id)?;
            let free = InternalNode::new(&buf)?.slotted().total_free();
            ensure!(
                free < SHORT_CELL.saturating_mul(2),
                "level {depth}, page {id}: {free} bytes free, node is not full"
            );
        }
    }

    // Leaves are full as well, so the whole file is close to the minimum:
    // 3000 entries of 34 bytes in 480-byte leaves is 14 per leaf. A split by
    // halves would need about twice as many pages.
    let leaves = all.last().context("no leaf level")?.len();
    ensure!(leaves <= 3000 / 14 + 2, "{leaves} leaves are too many");
    let total = usize::try_from(pager.page_count())?;
    ensure!(
        total <= leaves.saturating_add(leaves / 6).saturating_add(20),
        "{total} pages for {leaves} leaves"
    );
    ensure!(USABLE / SHORT_CELL > 30, "test assumes wide internal nodes");
    Ok(())
}

#[test]
fn inserts_into_a_hand_built_deep_tree_split_every_level() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    // Entries "key-00000", "key-00002", ...: the odd numbers are free.
    let entries = numbered_entries(1000);
    let root = bulk_load(&mut pager, &entries)?;
    let tree = BTree::open(&pager, root)?;
    let start = height(&pager, root)?;
    ensure!(start >= 4, "hand-built tree has only {start} levels");
    let internal_before = internal_count(&pager, root)?;

    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
    for i in 0..1000usize {
        let key = format!("key-{:05}", i.saturating_mul(2).saturating_add(1)).into_bytes();
        // Large values split the leaves again and again, so the parents fill.
        let value = vec![9u8; 100];
        ensure!(!tree.put(&mut pager, &key, &value)?, "key existed");
        model.insert(key, value);
        if i.checked_rem(100) == Some(0) {
            check_against(&pager, tree, root, &model, false)?;
        }
    }
    check_against(&pager, tree, root, &model, true)?;
    ensure!(
        internal_count(&pager, root)? > internal_before,
        "no internal node was split"
    );
    ensure_missing(get(tree, &pager, b"key-99999")?.as_deref(), "absent key")?;
    Ok(())
}

#[test]
fn a_hand_built_root_leaf_grows_deep() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let root = write_leaf(&mut pager, &[(b"a", b"1"), (b"b", b"2")], None)?;
    let tree = BTree::open(&pager, root)?;

    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    model.insert(b"a".to_vec(), b"1".to_vec());
    model.insert(b"b".to_vec(), b"2".to_vec());
    for i in 0..400usize {
        let value = b"v".to_vec();
        tree.put(&mut pager, &long_key(i), &value)?;
        model.insert(long_key(i), value);
    }
    check_against(&pager, tree, root, &model, true)?;
    ensure!(height(&pager, root)? >= 4, "the tree is not deep");
    Ok(())
}

#[test]
fn deep_tree_survives_reopening_the_file() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let root = tree.root();

    let (tallest, model) = grow(&mut pager, tree, &scrambled(641), long_key, 100)?;
    ensure!(tallest >= 4, "only reached height {tallest}");
    pager.sync()?;
    drop(pager);

    let pager = Pager::open(&path)?;
    let tree = BTree::open(&pager, root)?;
    check_against(&pager, tree, root, &model, true)
}

fn run_model(pool: &[Vec<u8>], ops: &[(usize, Vec<u8>)]) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let root = tree.root();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    for (step, (which, value)) in ops.iter().enumerate() {
        let key = pool
            .get(which.checked_rem(pool.len()).context("empty pool")?)
            .context("pool index")?;
        let replaced = tree.put(&mut pager, key, value)?;
        let expected = model.insert(key.clone(), value.clone()).is_some();
        ensure!(
            replaced == expected,
            "put {key:?}: {replaced} vs {expected}"
        );
        if step.checked_rem(40) == Some(0) {
            check_against(&pager, tree, root, &model, false)?;
        }
    }
    check_against(&pager, tree, root, &model, true)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn prop_deep_puts_match_btreemap_model(
        pool in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 60..=100),
            80..150,
        ),
        ops in prop::collection::vec(
            (any::<usize>(), prop::collection::vec(any::<u8>(), 0..=8)),
            200..450,
        ),
    ) {
        run_model(&pool, &ops).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
