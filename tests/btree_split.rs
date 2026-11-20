//! Tests for leaf splits in `BTree::put` (Phase 2, step 6). Page size is 512.
//!
//! Every entry has a 7-byte key and a 20-byte value, so a cell and its slot
//! use 34 bytes and a leaf holds 14 of them. `check_tree` (from
//! `btree_check`) validates the whole tree after the splits.
//!
//! The tests check:
//!
//! - the first split of the root leaf with ascending keys: the root page id
//!   stays, the root becomes an internal node with one separator, the left
//!   leaf keeps all its cells and the right leaf gets only the new cell;
//! - many ascending inserts leave every leaf but the last full;
//! - descending and random inserts split by bytes, so leaves are about half
//!   full, and the tree matches a `BTreeMap` model;
//! - the separator is the first key of the right leaf and a key equal to it
//!   is found there;
//! - a replace that grows a value in a full leaf splits it;
//! - a split of a leaf in the middle of the chain links the new leaf between
//!   the leaf and its old next leaf;
//! - a full parent splits and the tree grows to three levels (the deeper
//!   cases are in `btree_internal_split.rs`);
//! - a split tree survives closing and reopening the file;
//! - a property test against a `BTreeMap` model.

mod btree_check;
mod btree_support;

use std::collections::BTreeMap;

use anyhow::{Context, bail, ensure};
use btree_check::{Entries, check_tree, leaf_chain};
use btree_support::{
    PAGE_SIZE, blank, bulk_load, ensure_missing, ensure_value, get, height, new_pager,
    numbered_entries, temp_db, write_leaf,
};
use proptest::prelude::*;
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::btree::error::NodeError;
use storage_engine_rs::storage::btree::internal::InternalNode;
use storage_engine_rs::storage::btree::leaf::{LeafNode, LeafNodeMut};
use storage_engine_rs::storage::btree::node::ValueRef;
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::error::PageError;
use storage_engine_rs::storage::pager::types::PageId;

const VALUE: [u8; 20] = [7u8; 20];

/// Usable bytes of a node page: the page minus the 32-byte header.
const USABLE: usize = PAGE_SIZE - 32;

fn key(i: usize) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

fn add(a: usize, b: usize) -> anyhow::Result<usize> {
    a.checked_add(b).context("add overflow")
}

fn read_page(pager: &Pager, id: PageId) -> anyhow::Result<Vec<u8>> {
    let mut buf = blank();
    pager.read_page(id, &mut buf)?;
    Ok(buf)
}

fn is_internal(pager: &Pager, id: PageId) -> anyhow::Result<bool> {
    Ok(InternalNode::new(&read_page(pager, id)?).is_ok())
}

/// The cells of the leaf at `id`.
fn leaf_cells(pager: &Pager, id: PageId) -> anyhow::Result<Entries> {
    let buf = read_page(pager, id)?;
    let leaf = LeafNode::new(&buf)?;
    let mut cells = Vec::new();
    for index in 0..leaf.cell_count() {
        let cell = leaf.cell(index)?;
        match cell.value {
            ValueRef::Inline(value) => cells.push((cell.key.to_vec(), value.to_vec())),
            ValueRef::Overflow { .. } => bail!("unexpected overflow cell"),
        }
    }
    Ok(cells)
}

/// Bytes used by the cells and slots of the leaf at `id`.
fn leaf_used(pager: &Pager, id: PageId) -> anyhow::Result<usize> {
    let buf = read_page(pager, id)?;
    let free = LeafNode::new(&buf)?.slotted().total_free();
    USABLE
        .checked_sub(free)
        .context("free is larger than usable")
}

/// Whether the leaf at `id` cannot take another entry of the standard size.
fn is_full(pager: &Pager, id: PageId) -> anyhow::Result<bool> {
    let mut buf = read_page(pager, id)?;
    let mut leaf = LeafNodeMut::new(&mut buf)?;
    let end = leaf.view().cell_count();
    match leaf.insert(end, &key(999_999), ValueRef::Inline(&VALUE)) {
        Ok(()) => Ok(false),
        Err(NodeError::Page(PageError::PageFull { .. })) => Ok(true),
        Err(other) => bail!("unexpected error: {other:?}"),
    }
}

/// Puts the standard entry for every `i` in `order`, checking each is new.
fn put_all(
    tree: BTree,
    pager: &mut Pager,
    order: impl IntoIterator<Item = usize>,
) -> anyhow::Result<()> {
    for i in order {
        ensure!(!tree.put(pager, &key(i), &VALUE)?, "key {i} was not new");
    }
    Ok(())
}

/// Checks the tree against `expected` indexes: `check_tree` entries and `get`.
fn check_standard(pager: &Pager, tree: BTree, indexes: &[usize]) -> anyhow::Result<()> {
    let mut sorted = indexes.to_vec();
    sorted.sort_unstable();
    let expected: Entries = sorted.iter().map(|&i| (key(i), VALUE.to_vec())).collect();
    ensure!(check_tree(pager, tree)? == expected, "entries differ");
    for &i in indexes {
        ensure_value(get(tree, pager, &key(i))?.as_deref(), &VALUE, "get")?;
    }
    Ok(())
}

/// A scrambled order of `0..count` for a prime `count` (37 is coprime with it).
fn scrambled(count: usize) -> Vec<usize> {
    (0..count)
        .map(|i| i.saturating_mul(37).checked_rem(count).unwrap_or(0))
        .collect()
}

#[test]
fn first_split_of_the_root_leaf_is_a_right_only_append() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let root = tree.root();

    let mut count = 0usize;
    while !is_internal(&pager, root)? {
        put_all(tree, &mut pager, [count])?;
        count = add(count, 1)?;
        ensure!(count < 100, "the leaf never split");
    }

    ensure!(tree.root() == root, "root id changed");
    let buf = read_page(&pager, root)?;
    let node = InternalNode::new(&buf)?;
    node.validate()?;
    ensure!(
        node.cell_count() == 1,
        "root has {} keys",
        node.cell_count()
    );

    let left = node.child_at(0)?;
    let right = node.rightmost_child()?;
    ensure!(
        leaf_chain(&pager, tree)? == vec![left, right],
        "expected two leaves"
    );

    // The left leaf kept all its cells and the right one has only the new key.
    let last = key(count.checked_sub(1).context("no keys")?);
    let left_cells = leaf_cells(&pager, left)?;
    ensure!(
        add(left_cells.len(), 1)? == count,
        "left has {} cells",
        left_cells.len()
    );
    ensure!(is_full(&pager, left)?, "left leaf is not full");
    ensure!(
        leaf_cells(&pager, right)? == vec![(last.clone(), VALUE.to_vec())],
        "right leaf"
    );
    ensure!(node.key(0)? == last.as_slice(), "separator");

    check_standard(&pager, tree, &(0..count).collect::<Vec<_>>())
}

#[test]
fn ascending_inserts_fill_every_leaf_but_the_last() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;

    // 300 entries make about 22 leaves: the root can hold the separators.
    put_all(tree, &mut pager, 0..300)?;

    let chain = leaf_chain(&pager, tree)?;
    ensure!(chain.len() > 10, "only {} leaves", chain.len());
    let (last, others) = chain.split_last().context("no leaves")?;
    for id in others {
        ensure!(is_full(&pager, *id)?, "leaf {id} is not full");
    }
    ensure!(!leaf_cells(&pager, *last)?.is_empty(), "last leaf is empty");

    check_standard(&pager, tree, &(0..300).collect::<Vec<_>>())
}

#[test]
fn descending_inserts_split_by_bytes() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;

    put_all(tree, &mut pager, (0..200).rev())?;

    // A new smallest key never goes after the last cell, so every split is by
    // bytes and every leaf has at least a quarter of the page.
    let chain = leaf_chain(&pager, tree)?;
    ensure!(chain.len() > 10, "only {} leaves", chain.len());
    for id in &chain {
        let used = leaf_used(&pager, *id)?;
        ensure!(used >= USABLE / 4, "leaf {id} uses only {used} bytes");
    }

    check_standard(&pager, tree, &(0..200).collect::<Vec<_>>())
}

#[test]
fn random_inserts_keep_leaves_about_half_full() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;

    // 251 is prime, so the order visits every index once.
    let order = scrambled(251);
    ensure!(order.windows(2).any(|w| w.first() > w.get(1)), "sorted");
    put_all(tree, &mut pager, order.iter().copied())?;

    let chain = leaf_chain(&pager, tree)?;
    let mut used = 0usize;
    for id in &chain {
        used = add(used, leaf_used(&pager, *id)?)?;
    }
    // On average the leaves are over 40% full (random inserts give about 70%).
    let capacity = chain.len().checked_mul(USABLE).context("overflow")?;
    ensure!(
        used.checked_mul(10).context("overflow")? >= capacity.checked_mul(4).context("overflow")?,
        "{used} bytes in {} leaves",
        chain.len()
    );

    check_standard(&pager, tree, &order)
}

#[test]
fn separator_is_the_first_key_of_the_right_leaf() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let root = tree.root();

    // Descending keys: the first split is by bytes, in the middle.
    let mut next = 100usize;
    while !is_internal(&pager, root)? {
        put_all(tree, &mut pager, [next])?;
        next = next.checked_sub(1).context("the leaf never split")?;
    }

    let buf = read_page(&pager, root)?;
    let node = InternalNode::new(&buf)?;
    ensure!(node.cell_count() == 1, "one separator expected");
    let separator = node.key(0)?;
    let left_cells = leaf_cells(&pager, node.child_at(0)?)?;
    let right_cells = leaf_cells(&pager, node.rightmost_child()?)?;
    ensure!(
        left_cells.len() > 1 && right_cells.len() > 1,
        "not by bytes"
    );
    ensure!(
        right_cells.first().map(|c| c.0.as_slice()) == Some(separator),
        "separator is not the first key of the right leaf"
    );
    ensure!(
        left_cells.iter().all(|c| c.0.as_slice() < separator),
        "left leaf has a key not below the separator"
    );

    // The key equal to the separator is found, in the right leaf.
    ensure_value(
        get(tree, &pager, separator)?.as_deref(),
        &VALUE,
        "separator",
    )?;
    ensure!(
        right_cells.iter().any(|c| c.0.as_slice() == separator),
        "separator key is not in the right leaf"
    );
    check_tree(&pager, tree)?;
    Ok(())
}

#[test]
fn replace_that_grows_a_value_in_a_full_leaf_splits() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;

    // 14 entries fill the leaf: 14 * 34 = 476 of 480 bytes.
    let keys: Vec<Vec<u8>> = (0..14).map(key).collect();
    let cells: Vec<(&[u8], &[u8])> = keys.iter().map(|k| (k.as_slice(), &VALUE[..])).collect();
    let root = write_leaf(&mut pager, &cells, None)?;
    let tree = BTree::open(&pager, root)?;

    let big = [9u8; 100];
    let target = keys.get(6).context("key 6")?;
    ensure!(tree.put(&mut pager, target, &big)?, "not a replace");

    ensure!(
        tree.root() == root && is_internal(&pager, root)?,
        "no split"
    );
    ensure_value(get(tree, &pager, target)?.as_deref(), &big, "new value")?;
    let entries = check_tree(&pager, tree)?;
    ensure!(entries.len() == 14, "{} entries", entries.len());
    ensure!(
        entries.iter().any(|e| e.0 == *target && e.1 == big),
        "new value missing from the entries"
    );
    Ok(())
}

#[test]
fn split_in_the_middle_of_the_chain_links_the_new_leaf_between() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    // The loader packs leaves full: 200 entries make ten of them.
    let entries = numbered_entries(200);
    let root = bulk_load(&mut pager, &entries)?;
    let tree = BTree::open(&pager, root)?;
    ensure!(height(&pager, root)? == 3, "expected three levels");

    let before = leaf_chain(&pager, tree)?;
    ensure!(before.len() >= 4, "only {} leaves", before.len());
    let middle = *before.get(1).context("leaf 1")?;
    let first = leaf_cells(&pager, middle)?
        .into_iter()
        .next()
        .context("empty leaf")?
        .0;

    // A key just after the first key of the leaf, with a large value: the
    // full leaf cannot take it.
    let mut new_key = first;
    new_key.push(b'0');
    let value = [5u8; 40];
    ensure!(!tree.put(&mut pager, &new_key, &value)?, "key existed");

    let after = leaf_chain(&pager, tree)?;
    ensure!(
        after.len() == add(before.len(), 1)?,
        "one new leaf expected"
    );
    let new_leaf = *after.get(2).context("leaf 2")?;
    ensure!(!before.contains(&new_leaf), "leaf 2 is not new");
    ensure!(after.get(1) == Some(&middle), "left leaf moved");
    ensure!(
        after.get(3) == before.get(2),
        "old next is not after the new leaf"
    );
    ensure!(height(&pager, root)? == 3, "height changed");

    let all = check_tree(&pager, tree)?;
    ensure!(all.len() == 201, "{} entries", all.len());
    ensure_value(get(tree, &pager, &new_key)?.as_deref(), &value, "new key")?;
    for (k, v) in &entries {
        ensure_value(get(tree, &pager, k)?.as_deref(), v, "old key")?;
    }
    Ok(())
}

#[test]
fn full_parent_splits_instead_of_failing() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let root = tree.root();

    // Insert until the tree has three levels: the first full parent is the
    // root, and its split used to fail.
    let mut inserted = Vec::new();
    for i in 0..2000usize {
        ensure!(!tree.put(&mut pager, &key(i), &VALUE)?, "key {i} existed");
        inserted.push(i);
        if height(&pager, root)? == 3 {
            break;
        }
    }
    ensure!(height(&pager, root)? == 3, "the root never split");
    ensure!(tree.root() == root, "the root page id changed");
    check_standard(&pager, tree, &inserted)?;
    ensure_missing(get(tree, &pager, &key(999_999))?.as_deref(), "absent key")?;
    // A put that needs no split still works.
    ensure!(tree.put(&mut pager, &key(0), b"x")?, "replace failed");
    Ok(())
}

#[test]
fn split_tree_survives_reopening_the_file() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let root = tree.root();
    let order = scrambled(151);
    put_all(tree, &mut pager, order.iter().copied())?;
    ensure!(is_internal(&pager, root)?, "no split");
    pager.sync()?;
    drop(pager);

    let pager = Pager::open(&path)?;
    let tree = BTree::open(&pager, root)?;
    check_standard(&pager, tree, &order)
}

fn run_model(ops: &[(Vec<u8>, Vec<u8>)]) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    for (step, (key, value)) in ops.iter().enumerate() {
        let replaced = tree.put(&mut pager, key, value)?;
        let expected = model.insert(key.clone(), value.clone()).is_some();
        ensure!(
            replaced == expected,
            "put {key:?}: tree {replaced}, model {expected}"
        );

        if step.checked_rem(5) == Some(0) {
            let entries = check_tree(&pager, tree)?;
            let expected: Entries = model.clone().into_iter().collect();
            ensure!(entries == expected, "entries differ at step {step}");
        }
    }

    let entries = check_tree(&pager, tree)?;
    let expected: Entries = model.clone().into_iter().collect();
    ensure!(entries == expected, "entries differ at the end");
    for (k, v) in &model {
        ensure_value(get(tree, &pager, k)?.as_deref(), v, "model key")?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_puts_match_btreemap_model(
        ops in prop::collection::vec(
            (
                prop::collection::vec(any::<u8>(), 1..=24),
                prop::collection::vec(any::<u8>(), 0..=24),
            ),
            1..200,
        ),
    ) {
        run_model(&ops).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
