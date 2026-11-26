//! Tests for `BTree::delete` (Phase 2, step 10): the delete itself. Since steps
//! 11a and 11b underfull nodes are rebalanced and the tree shrinks, so the
//! tests that empty leaves allow merged leaves and a collapsed tree.
//! Page size is 512 bytes.
//!
//! The tests check:
//!
//! - deleting from an empty tree and a missing key writes nothing, and a key
//!   longer than `max_key_len` is a plain miss;
//! - deleting an inline key removes only that key, and a second delete of the
//!   same key returns `false`;
//! - deleting an overflow value frees its whole chain, the free list stays
//!   valid, no page leaks, and the freed pages are reused by the next large
//!   value;
//! - deleting every key of a three-level tree collapses it to an empty root
//!   leaf (same page id), with every other page free, and every scan is
//!   empty;
//! - emptying the first, a middle and the last leaf: scans that start where
//!   its keys were match the model in both directions;
//! - leaves emptied by hand, like the ones that step 10 code left in a file:
//!   scans skip them in both directions;
//! - deleting a key that is a separator in the root and putting it back;
//! - refilling an emptied leaf until it splits;
//! - a corrupt overflow chain: the delete fails after the leaf was written,
//!   so the key is gone;
//! - a property test against a `BTreeMap` model with random puts and deletes.
//!
//! The compile-fail check (no `delete` while a cursor is alive) is a doctest
//! on `BTree::delete`.

mod btree_check;
#[path = "support/db.rs"]
mod db;
#[path = "support/leaks.rs"]
mod leaks;
#[path = "support/lookup.rs"]
mod lookup;
#[path = "support/shape.rs"]
mod shape;

use std::collections::BTreeMap;
use std::ops::Bound::{self, Excluded, Included, Unbounded};

use anyhow::{Context, bail, ensure};
use btree_check::{Entries, check_tree, leaf_chain};
use db::{new_pager, temp_db};
use leaks::ensure_no_leaks;
use lookup::{ensure_missing, ensure_value, get};
use proptest::prelude::*;
use shape::{blank, height, numbered_entries};
use storage_engine_rs::storage::btree::error::{BTreeError, Violation};
use storage_engine_rs::storage::btree::internal::InternalNode;
use storage_engine_rs::storage::btree::leaf::{LeafNode, LeafNodeMut};
use storage_engine_rs::storage::btree::node::max_key_len;
use storage_engine_rs::storage::btree::overflow::{overflow_capacity, read_value};
use storage_engine_rs::storage::btree::{BTree, Direction, verify_tree};
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::types::PageId;

/// An owned bound, so a test can build it and lend it to `range`.
type Bnd = Bound<Vec<u8>>;

const BOTH: [Direction; 2] = [Direction::Forward, Direction::Reverse];

const KEY: &[u8] = b"key";

const fn borrow(bound: &Bnd) -> Bound<&[u8]> {
    match bound {
        Unbounded => Unbounded,
        Included(key) => Included(key.as_slice()),
        Excluded(key) => Excluded(key.as_slice()),
    }
}

fn model_of(entries: &Entries) -> BTreeMap<Vec<u8>, Vec<u8>> {
    entries.iter().cloned().collect()
}

/// Runs a scan to the end and returns the entries, with the values read in
/// full. Also checks that the cursor is fused.
fn scan(
    tree: BTree,
    pager: &Pager,
    lower: &Bnd,
    upper: &Bnd,
    direction: Direction,
) -> anyhow::Result<Entries> {
    let mut cursor = tree.range(pager, borrow(lower), borrow(upper), direction)?;
    let mut out = Vec::new();
    while let Some(cell) = cursor.next_entry()? {
        let mut value = Vec::new();
        read_value(pager, cell.value, &mut value)?;
        out.push((cell.key.to_vec(), value));
    }
    ensure!(cursor.next_entry()?.is_none(), "cursor is not fused");
    Ok(out)
}

/// What a scan must return, computed from the model.
fn expected(
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    lower: &Bnd,
    upper: &Bnd,
    direction: Direction,
) -> Entries {
    let above = |key: &Vec<u8>| match lower {
        Unbounded => true,
        Included(bound) => key >= bound,
        Excluded(bound) => key > bound,
    };
    let below = |key: &Vec<u8>| match upper {
        Unbounded => true,
        Included(bound) => key <= bound,
        Excluded(bound) => key < bound,
    };
    let mut entries: Entries = model
        .iter()
        .filter(|(key, _)| above(key) && below(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if direction == Direction::Reverse {
        entries.reverse();
    }
    entries
}

fn check_range(
    tree: BTree,
    pager: &Pager,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    lower: &Bnd,
    upper: &Bnd,
) -> anyhow::Result<()> {
    for direction in BOTH {
        let got = scan(tree, pager, lower, upper, direction)?;
        ensure!(
            got == expected(model, lower, upper, direction),
            "{direction:?} {lower:?}..{upper:?}: {} entries differ from the model",
            got.len()
        );
    }
    Ok(())
}

/// Every combination of bound kinds on two keys.
fn bound_pairs(low: &[u8], high: &[u8]) -> Vec<(Bnd, Bnd)> {
    let lowers = [Unbounded, Included(low.to_vec()), Excluded(low.to_vec())];
    let uppers = [Unbounded, Included(high.to_vec()), Excluded(high.to_vec())];
    let mut pairs = Vec::new();
    for lower in &lowers {
        for upper in &uppers {
            pairs.push((lower.clone(), upper.clone()));
        }
    }
    pairs
}

/// A tree of `count` numbered entries, put in a scrambled order. Returns the
/// tree and the entries in key order.
fn put_tree(pager: &mut Pager, count: usize) -> anyhow::Result<(BTree, Entries)> {
    let tree = BTree::create(pager)?;
    let entries = numbered_entries(count);
    for step in 0..count {
        let index = scramble(step, count)?;
        let (key, value) = entries.get(index).context("entry")?;
        tree.put(pager, key, value)?;
    }
    Ok((tree, entries))
}

/// A permutation of `0..count` (`count` is not a multiple of 7919).
fn scramble(step: usize, count: usize) -> anyhow::Result<usize> {
    step.checked_mul(7919)
        .and_then(|n| n.checked_rem(count))
        .context("overflow")
}

fn read(pager: &Pager, id: PageId) -> anyhow::Result<Vec<u8>> {
    let mut buf = blank();
    pager.read_page(id, &mut buf)?;
    Ok(buf)
}

/// The keys of leaf `id`, in order.
fn leaf_keys(pager: &Pager, id: PageId) -> anyhow::Result<Vec<Vec<u8>>> {
    let buf = read(pager, id)?;
    let leaf = LeafNode::new(&buf)?;
    let mut keys = Vec::new();
    for index in 0..leaf.cell_count() {
        keys.push(leaf.cell(index)?.key.to_vec());
    }
    Ok(keys)
}

/// The leaf of the chain that holds `key`.
fn leaf_of(pager: &Pager, tree: BTree, key: &[u8]) -> anyhow::Result<PageId> {
    for id in leaf_chain(pager, tree)? {
        if leaf_keys(pager, id)?.iter().any(|k| k == key) {
            return Ok(id);
        }
    }
    bail!("no leaf holds {key:?}")
}

/// `true` if `after` is `before` with some leaves removed (merges free the
/// right leaf of a pair and keep the order of the others).
fn is_subsequence(after: &[PageId], before: &[PageId]) -> bool {
    let mut rest = before.iter();
    after.iter().all(|id| rest.any(|b| b == id))
}

/// `len` bytes that depend on the position and on `seed`.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| {
            u8::try_from(i.wrapping_mul(31).wrapping_add(usize::from(seed)) % 251).unwrap_or(0)
        })
        .collect()
}

// ------------------------------------------------------------ small trees

#[test]
fn delete_from_an_empty_tree_writes_nothing() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let size = pager.page_count();
    let free = pager.free_page_count();
    let root = read(&pager, tree.root())?;

    ensure!(
        !tree.delete(&mut pager, b"missing")?,
        "deleted a missing key"
    );

    ensure!(pager.page_count() == size, "page count changed");
    ensure!(pager.free_page_count() == free, "free count changed");
    ensure!(read(&pager, tree.root())? == root, "root changed");
    Ok(())
}

#[test]
fn delete_an_inline_key_keeps_its_neighbours() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    for key in [b"a", b"b", b"c"] {
        tree.put(&mut pager, key, key)?;
    }

    ensure!(tree.delete(&mut pager, b"b")?, "key was not deleted");
    ensure_missing(get(tree, &pager, b"b")?.as_deref(), "b")?;
    ensure_value(get(tree, &pager, b"a")?.as_deref(), b"a", "a")?;
    ensure_value(get(tree, &pager, b"c")?.as_deref(), b"c", "c")?;

    ensure!(
        !tree.delete(&mut pager, b"b")?,
        "second delete returned true"
    );
    let entries = check_tree(&pager, tree)?;
    ensure!(
        entries
            == vec![
                (b"a".to_vec(), b"a".to_vec()),
                (b"c".to_vec(), b"c".to_vec())
            ],
        "entries"
    );
    Ok(())
}

#[test]
fn delete_a_missing_key_between_existing_keys_leaves_the_leaf_identical() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    tree.put(&mut pager, b"a", b"1")?;
    tree.put(&mut pager, b"c", b"3")?;
    let before = read(&pager, tree.root())?;

    ensure!(!tree.delete(&mut pager, b"b")?, "deleted a missing key");

    ensure!(read(&pager, tree.root())? == before, "leaf bytes changed");
    Ok(())
}

#[test]
fn a_key_longer_than_the_maximum_is_a_miss() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    tree.put(&mut pager, b"a", b"1")?;
    let before = read(&pager, tree.root())?;

    let long = vec![b'a'; max_key_len(db::page_size()?).saturating_add(1)];
    ensure!(!tree.delete(&mut pager, &long)?, "deleted a too long key");

    ensure!(read(&pager, tree.root())? == before, "leaf bytes changed");
    Ok(())
}

// ------------------------------------------------------- overflow values

#[test]
fn delete_an_overflow_value_frees_its_chain_and_the_pages_are_reused() -> anyhow::Result<()> {
    let capacity = overflow_capacity(db::page_size()?);
    let len = capacity
        .checked_mul(3)
        .and_then(|n| n.checked_add(10))
        .context("overflow")?;
    let chain_len = len.div_ceil(capacity);

    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    tree.put(&mut pager, b"small", b"1")?;
    tree.put(&mut pager, KEY, &pattern(len, 4))?;
    let free_before = usize::try_from(pager.free_page_count())?;
    let (chain, _) = ensure_no_leaks(&pager, tree)?;
    ensure!(
        chain == chain_len,
        "chain has {chain} pages, expected {chain_len}"
    );

    ensure!(tree.delete(&mut pager, KEY)?, "key was not deleted");

    let free_after = usize::try_from(pager.free_page_count())?;
    ensure!(
        free_after == free_before.checked_add(chain_len).context("overflow")?,
        "free pages: {free_before} before, {free_after} after, chain of {chain_len}"
    );
    let (chain, entries) = ensure_no_leaks(&pager, tree)?;
    ensure!(chain == 0, "{chain} overflow pages left");
    ensure!(
        entries == vec![(b"small".to_vec(), b"1".to_vec())],
        "entries"
    );

    // The next large value reuses the freed pages: the file does not grow.
    let page_count = pager.page_count();
    let value = pattern(len, 5);
    tree.put(&mut pager, KEY, &value)?;
    ensure!(pager.page_count() == page_count, "the file grew");
    ensure_value(get(tree, &pager, KEY)?.as_deref(), &value, "reused chain")?;
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

// ------------------------------------------------------ multi-level trees

#[test]
fn delete_every_key_of_a_three_level_tree() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let count = 1000;
    let (tree, entries) = put_tree(&mut pager, count)?;
    let root = tree.root();
    ensure!(height(&pager, root)? >= 3, "tree is too shallow");

    for step in 0..count {
        let (key, _) = entries.get(scramble(step, count)?).context("entry")?;
        ensure!(tree.delete(&mut pager, key)?, "key {key:?} was not deleted");
    }

    for (key, _) in &entries {
        ensure_missing(get(tree, &pager, key)?.as_deref(), "deleted key")?;
    }
    // Every merge and collapse (steps 11a and 11b) leaves one empty root leaf
    // with the same page id, and every other page is free.
    ensure!(tree.root() == root, "root id changed");
    ensure!(height(&pager, root)? == 1, "the tree did not collapse");
    ensure!(leaf_chain(&pager, tree)? == [root], "leaf chain");
    ensure!(check_tree(&pager, tree)?.is_empty(), "entries left");
    verify_tree(&pager, tree)?;
    let others = pager.page_count().checked_sub(2).context("pages")?;
    ensure!(others == pager.free_page_count(), "pages are not free");
    ensure_no_leaks(&pager, tree)?;
    for (lower, upper) in bound_pairs(b"key-00100", b"key-00900") {
        check_range(tree, &pager, &BTreeMap::new(), &lower, &upper)?;
    }
    Ok(())
}

/// Empties the leaf at `position` of the chain of a 1000-entry tree and
/// checks the tree and the scans that start or end inside the empty leaf.
fn empty_leaf_at(position: Position) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, entries) = put_tree(&mut pager, 1000)?;
    let leaves = leaf_chain(&pager, tree)?;
    let index = match position {
        Position::First => 0,
        Position::Middle => leaves.len() / 2,
        Position::Last => leaves.len().checked_sub(1).context("no leaves")?,
    };
    let target = *leaves.get(index).context("leaf")?;
    let doomed = leaf_keys(&pager, target)?;
    ensure!(doomed.len() > 1, "the leaf has {} keys", doomed.len());

    let mut model = model_of(&entries);
    for key in &doomed {
        ensure!(tree.delete(&mut pager, key)?, "key {key:?} was not deleted");
        model.remove(key);
    }

    // The emptied leaf may have been merged into a sibling (step 11a), so its
    // page can be free; the other leaves keep their order. The scans below
    // still cross the place where its keys were.
    ensure!(
        is_subsequence(&leaf_chain(&pager, tree)?, &leaves),
        "leaf chain changed"
    );
    let remaining: Entries = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    ensure!(check_tree(&pager, tree)? == remaining, "check_tree");

    let first = doomed.first().context("first")?;
    let last = doomed.last().context("last")?;
    let middle = doomed.get(doomed.len() / 2).context("middle")?;
    let top = entries.last().map_or(&b""[..], |(key, _)| key.as_slice());
    for (low, high) in [
        (first, last),
        (first, middle),
        (middle, last),
        (middle, middle),
    ] {
        for (lower, upper) in bound_pairs(low, high) {
            check_range(tree, &pager, &model, &lower, &upper)?;
        }
    }
    for (lower, upper) in bound_pairs(middle, top) {
        check_range(tree, &pager, &model, &lower, &upper)?;
    }
    for (lower, upper) in bound_pairs(b"", middle) {
        check_range(tree, &pager, &model, &lower, &upper)?;
    }
    check_range(tree, &pager, &model, &Unbounded, &Unbounded)
}

#[derive(Clone, Copy)]
enum Position {
    First,
    Middle,
    Last,
}

#[test]
fn an_emptied_first_leaf_is_skipped_by_scans() -> anyhow::Result<()> {
    empty_leaf_at(Position::First)
}

#[test]
fn an_emptied_middle_leaf_is_skipped_by_scans() -> anyhow::Result<()> {
    empty_leaf_at(Position::Middle)
}

#[test]
fn an_emptied_last_leaf_is_skipped_by_scans() -> anyhow::Result<()> {
    empty_leaf_at(Position::Last)
}

/// Rewrites the leaf at `position` of a 1000-entry tree as an empty leaf that
/// is still linked and still has its separators in the parents: the shape that
/// `delete` of step 10 left in a file. The cursor must skip it.
fn hand_emptied_leaf_at(position: Position) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, entries) = put_tree(&mut pager, 1000)?;
    let leaves = leaf_chain(&pager, tree)?;
    let index = match position {
        Position::First => 0,
        Position::Middle => leaves.len() / 2,
        Position::Last => leaves.len().checked_sub(1).context("no leaves")?,
    };
    let target = *leaves.get(index).context("leaf")?;
    let doomed = leaf_keys(&pager, target)?;
    ensure!(doomed.len() > 1, "the leaf has {} keys", doomed.len());

    let next = LeafNode::new(&read(&pager, target)?)?.next_leaf()?;
    let mut buf = blank();
    LeafNodeMut::init(&mut buf)?;
    LeafNodeMut::new(&mut buf)?.set_next_leaf(next)?;
    pager.write_page(target, &mut buf)?;

    let mut model = model_of(&entries);
    for key in &doomed {
        model.remove(key);
    }
    ensure!(leaf_chain(&pager, tree)? == leaves, "leaf chain");
    ensure!(leaf_keys(&pager, target)?.is_empty(), "leaf is not empty");
    // The tree is invalid on purpose, so the scans below are the check.
    ensure!(
        matches!(
            verify_tree(&pager, tree),
            Err(BTreeError::Invariant { page, violation: Violation::EmptyLeaf }) if page == target
        ),
        "verify_tree did not report the empty leaf"
    );

    let first = doomed.first().context("first")?;
    let last = doomed.last().context("last")?;
    for (low, high) in [(first, last), (last, last), (first, first)] {
        for (lower, upper) in bound_pairs(low, high) {
            check_range(tree, &pager, &model, &lower, &upper)?;
        }
    }
    check_range(tree, &pager, &model, &Unbounded, &Unbounded)
}

#[test]
fn scans_skip_a_hand_emptied_first_middle_and_last_leaf() -> anyhow::Result<()> {
    hand_emptied_leaf_at(Position::First)?;
    hand_emptied_leaf_at(Position::Middle)?;
    hand_emptied_leaf_at(Position::Last)
}

#[test]
fn delete_a_separator_key_and_put_it_again() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, entries) = put_tree(&mut pager, 1000)?;
    ensure!(height(&pager, tree.root())? >= 3, "tree is too shallow");
    let leaves = leaf_chain(&pager, tree)?;

    // A key of the root is the first key of a leaf that was split off.
    let root = read(&pager, tree.root())?;
    let separator = InternalNode::new(&root)?.key(0)?.to_vec();
    let leaf = leaf_of(&pager, tree, &separator)?;
    let value = get(tree, &pager, &separator)?.context("separator is not a key")?;

    ensure!(tree.delete(&mut pager, &separator)?, "not deleted");
    ensure_missing(get(tree, &pager, &separator)?.as_deref(), "separator")?;
    ensure!(
        check_tree(&pager, tree)?.len() == 999,
        "entries after delete"
    );

    ensure!(!tree.put(&mut pager, &separator, &value)?, "put replaced");
    ensure!(check_tree(&pager, tree)? == entries, "check_tree after put");
    ensure_value(get(tree, &pager, &separator)?.as_deref(), &value, "back")?;
    ensure!(
        leaf_of(&pager, tree, &separator)? == leaf,
        "the key landed in another leaf"
    );
    ensure!(leaf_chain(&pager, tree)? == leaves, "leaf chain changed");
    Ok(())
}

#[test]
fn refilling_an_emptied_leaf_until_it_splits() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, entries) = put_tree(&mut pager, 1000)?;
    let leaves = leaf_chain(&pager, tree)?;
    let target = *leaves.get(leaves.len() / 2).context("leaf")?;
    let doomed = leaf_keys(&pager, target)?;
    let mut model = model_of(&entries);
    for key in &doomed {
        ensure!(tree.delete(&mut pager, key)?, "not deleted");
        model.remove(key);
    }

    // `base-NNN` sorts after `base` and before the next numbered key, so
    // every new key belongs to the emptied leaf.
    let base = doomed.first().context("first")?;
    for i in 0..60 {
        let mut key = base.clone();
        key.extend_from_slice(format!("-{i:03}").as_bytes());
        let value = pattern(40, 7);
        ensure!(!tree.put(&mut pager, &key, &value)?, "put replaced");
        model.insert(key, value);
    }

    let now = leaf_chain(&pager, tree)?;
    ensure!(now.len() > leaves.len(), "the leaf did not split");
    let all: Entries = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    ensure!(check_tree(&pager, tree)? == all, "check_tree");
    check_range(tree, &pager, &model, &Unbounded, &Unbounded)
}

// ---------------------------------------------------------- corrupt chain

#[test]
fn a_corrupt_chain_fails_the_delete_after_the_key_is_gone() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let len = overflow_capacity(db::page_size()?)
        .checked_mul(2)
        .and_then(|n| n.checked_add(100))
        .context("overflow")?;
    tree.put(&mut pager, KEY, &pattern(len, 9))?;

    // The chain is pages 2, 3 and 4. Page 3 becomes a leaf.
    let mut buf = blank();
    LeafNodeMut::init(&mut buf)?;
    pager.write_page(PageId::new(3), &mut buf)?;

    match tree.delete(&mut pager, KEY) {
        Err(BTreeError::CorruptTree { page, .. }) => {
            ensure!(page == PageId::new(3), "page {page}");
        }
        other => bail!("expected CorruptTree, got {other:?}"),
    }

    ensure_missing(get(tree, &pager, KEY)?.as_deref(), "deleted key")?;
    ensure!(check_tree(&pager, tree)?.is_empty(), "entries left");
    ensure!(pager.free_page_count() == 1, "page 2 should be freed");
    Ok(())
}

// --------------------------------------------------------------- proptest

/// A bound as the property test draws it: a kind (0 unbounded, 1 included,
/// 2 excluded) and a key.
type Spec = (u8, Vec<u8>);

fn bound_strategy() -> impl Strategy<Value = Spec> {
    (
        0u8..3,
        prop::collection::vec(prop::sample::select(vec![b'a', b'b', b'c']), 0..6),
    )
}

fn make_bound((kind, key): &Spec) -> Bnd {
    match kind {
        0 => Unbounded,
        1 => Included(key.clone()),
        _ => Excluded(key.clone()),
    }
}

/// One operation: a selector (put about 60% of the time, delete otherwise),
/// a key index, a value length and a seed.
type Op = (u8, usize, usize, u8);

fn run_model(keys: &[Vec<u8>], ops: &[Op], bounds: &[(Spec, Spec)]) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    for &(selector, key_index, len, seed) in ops {
        let key = keys
            .get(key_index.checked_rem(keys.len()).context("no keys")?)
            .context("key")?;
        if selector % 10 < 6 {
            let value = pattern(len, seed);
            tree.put(&mut pager, key, &value)?;
            model.insert(key.clone(), value);
        } else {
            let deleted = tree.delete(&mut pager, key)?;
            ensure!(
                deleted == model.remove(key).is_some(),
                "delete of {key:?} returned {deleted}"
            );
            ensure_missing(get(tree, &pager, key)?.as_deref(), "deleted key")?;
        }
    }

    let all: Entries = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    verify_tree(&pager, tree)?;
    let (_, entries) = ensure_no_leaks(&pager, tree)?;
    ensure!(entries == all, "entries differ from the model");
    check_range(tree, &pager, &model, &Unbounded, &Unbounded)?;
    for (lower, upper) in bounds {
        check_range(tree, &pager, &model, &make_bound(lower), &make_bound(upper))?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn prop_puts_and_deletes_match_the_model(
        keys in prop::collection::vec(
            prop::collection::vec(prop::sample::select(vec![b'a', b'b', b'c']), 0..6),
            1..=40,
        ),
        ops in prop::collection::vec(
            (
                any::<u8>(),
                any::<usize>(),
                prop_oneof![0usize..120, 100usize..1200, 480usize..500],
                any::<u8>(),
            ),
            1..160,
        ),
        bounds in prop::collection::vec((bound_strategy(), bound_strategy()), 1..8),
    ) {
        run_model(&keys, &ops, &bounds).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
