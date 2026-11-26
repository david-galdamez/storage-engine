//! Tests for `BTree::range` and `Cursor` (Phase 2, step 9): ordered range
//! scans, forward and in reverse. Page size is 512 bytes.
//!
//! The tests check:
//!
//! - an empty tree and a single-leaf tree, with every bound combination;
//! - full scans of a three-level tree in both directions;
//! - every bound kind on each end, with the bound present, absent, equal to a
//!   separator (the first key of a leaf), equal to the last key of a leaf,
//!   below the minimum and above the maximum;
//! - empty and inverted ranges return nothing and are not errors;
//! - overflow values come back as references and are read with `read_value`,
//!   and a key-only scan never reads a chain;
//! - hand-built trees with empty leaves at the start, in the middle and at the
//!   end (also across parents) are skipped in both directions;
//! - a `next_leaf` cycle and a `next_leaf` to a non-leaf page are errors, not
//!   hangs, and the cursor returns `None` after an error;
//! - a scan after closing and reopening the file;
//! - a property test against a `BTreeMap` model.
//!
//! The compile-fail check (no `put` while a cursor is alive) is a doctest on
//! `Cursor`.

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
use std::ops::Bound::{self, Excluded, Included, Unbounded};

use anyhow::{Context, bail, ensure};
use btree_check::{Entries, check_tree, leaf_chain};
use build::{bulk_load, write_internal, write_leaf};
use db::{new_pager, temp_db};
use lookup::{ensure_missing, ensure_value, get};
use proptest::prelude::*;
use shape::{blank, height, numbered_entries};
use storage_engine_rs::storage::btree::error::BTreeError;
use storage_engine_rs::storage::btree::leaf::{LeafNode, LeafNodeMut};
use storage_engine_rs::storage::btree::node::ValueRef;
use storage_engine_rs::storage::btree::overflow::read_value;
use storage_engine_rs::storage::btree::{BTree, Cursor, Direction};
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::types::PageId;

/// An owned bound, so a test can build it and lend it to `range`.
type Bnd = Bound<Vec<u8>>;

const BOTH: [Direction; 2] = [Direction::Forward, Direction::Reverse];

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
        let index = step
            .checked_mul(7919)
            .and_then(|n| n.checked_rem(count))
            .context("overflow")?;
        let (key, value) = entries.get(index).context("entry")?;
        tree.put(pager, key, value)?;
    }
    Ok((tree, entries))
}

// ------------------------------------------------------------ small trees

#[test]
fn an_empty_tree_returns_nothing_for_every_bound() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let model = BTreeMap::new();

    for (lower, upper) in bound_pairs(b"a", b"z") {
        for direction in BOTH {
            ensure!(
                scan(tree, &pager, &lower, &upper, direction)?.is_empty(),
                "{direction:?} {lower:?}..{upper:?}"
            );
        }
        check_range(tree, &pager, &model, &lower, &upper)?;
    }
    Ok(())
}

#[test]
fn a_single_leaf_scans_in_both_directions() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, entries) = put_tree(&mut pager, 6)?;
    ensure!(height(&pager, tree.root())? == 1, "expected one leaf");
    let model = model_of(&entries);

    let all = scan(tree, &pager, &Unbounded, &Unbounded, Direction::Forward)?;
    ensure!(all == entries, "forward");
    let mut reversed = entries;
    reversed.reverse();
    let all = scan(tree, &pager, &Unbounded, &Unbounded, Direction::Reverse)?;
    ensure!(all == reversed, "reverse");

    let keys: [&[u8]; 5] = [b"", b"key-00000", b"key-00003", b"key-00004", b"zzz"];
    for key in keys {
        for (lower, upper) in bound_pairs(key, b"key-00008") {
            check_range(tree, &pager, &model, &lower, &upper)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------ multi-level trees

#[test]
fn a_three_level_tree_scans_fully_in_both_directions() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, entries) = put_tree(&mut pager, 1000)?;
    ensure!(height(&pager, tree.root())? >= 3, "tree is too shallow");
    ensure!(check_tree(&pager, tree)? == entries, "checker");

    let forward = scan(tree, &pager, &Unbounded, &Unbounded, Direction::Forward)?;
    ensure!(forward == entries, "forward scan differs");
    let mut reversed = entries;
    reversed.reverse();
    let reverse = scan(tree, &pager, &Unbounded, &Unbounded, Direction::Reverse)?;
    ensure!(reverse == reversed, "reverse scan differs");

    ensure_missing(get(tree, &pager, b"zzz")?.as_deref(), "unrelated key")?;
    Ok(())
}

/// First and last key of a leaf page.
fn leaf_ends(pager: &Pager, id: PageId) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = blank();
    pager.read_page(id, &mut buf)?;
    let leaf = LeafNode::new(&buf)?;
    let last = leaf.cell_count().checked_sub(1).context("empty leaf")?;
    Ok((leaf.key(0)?.to_vec(), leaf.key(last)?.to_vec()))
}

#[test]
fn bounds_at_leaf_edges_and_outside_the_keys() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, entries) = put_tree(&mut pager, 1000)?;
    let model = model_of(&entries);

    // For each leaf: its first key (a separator), its last key, and an absent
    // key just after the first one. Plus keys below and above everything.
    let mut probes: Vec<Vec<u8>> = vec![Vec::new(), b"zzz".to_vec()];
    for id in leaf_chain(&pager, tree)? {
        let (first, last) = leaf_ends(&pager, id)?;
        let mut absent = first.clone();
        absent.push(0);
        probes.extend([first, last, absent]);
    }

    // One bound at a time.
    for probe in &probes {
        for bound in [Included(probe.clone()), Excluded(probe.clone())] {
            check_range(tree, &pager, &model, &bound, &Unbounded)?;
            check_range(tree, &pager, &model, &Unbounded, &bound)?;
        }
    }

    // Two bounds, on a sample of the probes.
    let sample: Vec<&Vec<u8>> = probes.iter().step_by(37).collect();
    for low in &sample {
        for high in &sample {
            for (lower, upper) in bound_pairs(low, high) {
                check_range(tree, &pager, &model, &lower, &upper)?;
            }
        }
    }
    Ok(())
}

#[test]
fn empty_and_inverted_ranges_return_nothing() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, entries) = put_tree(&mut pager, 300)?;
    let low = entries.get(50).context("entry")?.0.clone();
    let high = entries.get(200).context("entry")?.0.clone();

    for direction in BOTH {
        let one = scan(
            tree,
            &pager,
            &Included(low.clone()),
            &Included(low.clone()),
            direction,
        )?;
        ensure!(
            one.len() == 1 && one.first().map(|e| &e.0) == Some(&low),
            "k..=k"
        );

        let cases = [
            (Included(high.clone()), Included(low.clone())),
            (Excluded(high.clone()), Excluded(low.clone())),
            (Excluded(low.clone()), Excluded(low.clone())),
            (Excluded(low.clone()), Included(low.clone())),
            (Included(low.clone()), Excluded(low.clone())),
        ];
        for (lower, upper) in &cases {
            ensure!(
                scan(tree, &pager, lower, upper, direction)?.is_empty(),
                "{direction:?} {lower:?}..{upper:?}"
            );
        }
    }
    Ok(())
}

// --------------------------------------------------------------- overflow

#[test]
fn overflow_values_are_references_and_key_scans_skip_the_chains() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let mut model = BTreeMap::new();
    for n in 0..12usize {
        let key = format!("k{n:02}").into_bytes();
        let len = if n % 2 == 0 { 1500 } else { 20 };
        let byte = u8::try_from(n)?;
        let value = vec![byte; len];
        tree.put(&mut pager, &key, &value)?;
        model.insert(key, value);
    }

    for direction in BOTH {
        let got = scan(tree, &pager, &Unbounded, &Unbounded, direction)?;
        ensure!(
            got == expected(&model, &Unbounded, &Unbounded, direction),
            "{direction:?}"
        );
    }

    // Even keys are in overflow chains.
    let mut cursor = tree.range(&pager, Unbounded, Unbounded, Direction::Forward)?;
    let mut first_chain = None;
    let mut seen = 0usize;
    while let Some(cell) = cursor.next_entry()? {
        let overflow = matches!(cell.value, ValueRef::Overflow { .. });
        ensure!(
            overflow == seen.is_multiple_of(2),
            "cell {seen}: wrong value kind"
        );
        if let ValueRef::Overflow { first_page, .. } = cell.value {
            first_chain.get_or_insert(first_page);
        }
        seen += 1;
    }
    ensure!(seen == 12, "expected 12 cells, got {seen}");

    // A corrupt chain does not break a scan that does not read values.
    let chain = first_chain.context("no overflow cell")?;
    pager.write_page(chain, &mut blank())?;
    for direction in BOTH {
        let mut cursor = tree.range(&pager, Unbounded, Unbounded, direction)?;
        let mut keys = 0usize;
        while cursor.next_entry()?.is_some() {
            keys += 1;
        }
        ensure!(keys == 12, "{direction:?}: {keys} keys");
    }
    ensure!(
        scan(tree, &pager, &Unbounded, &Unbounded, Direction::Forward).is_err(),
        "reading the corrupt chain must fail"
    );
    Ok(())
}

// ------------------------------------------------- hand-built empty leaves

fn strs(entries: &[(&str, &str)]) -> Entries {
    entries
        .iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect()
}

fn write_leaf_owned(
    pager: &mut Pager,
    entries: &Entries,
    next: Option<PageId>,
) -> anyhow::Result<PageId> {
    let cells: Vec<(&[u8], &[u8])> = entries
        .iter()
        .map(|(k, v)| (k.as_slice(), v.as_slice()))
        .collect();
    write_leaf(pager, &cells, next)
}

fn check_all_ranges(tree: BTree, pager: &Pager, entries: &Entries) -> anyhow::Result<()> {
    let model = model_of(entries);
    for key in ["", "a", "b", "c", "ca", "d", "e", "f", "zz"] {
        for (lower, upper) in bound_pairs(key.as_bytes(), b"d") {
            check_range(tree, pager, &model, &lower, &upper)?;
        }
    }
    Ok(())
}

#[test]
fn empty_leaves_are_skipped() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    // Built right to left so that each leaf knows its next leaf. Leaves 0, 2
    // and 4 are empty.
    let l4 = write_leaf_owned(&mut pager, &Vec::new(), None)?;
    let l3 = write_leaf_owned(&mut pager, &strs(&[("d", "D"), ("e", "E")]), Some(l4))?;
    let l2 = write_leaf_owned(&mut pager, &Vec::new(), Some(l3))?;
    let l1 = write_leaf_owned(&mut pager, &strs(&[("a", "A"), ("b", "B")]), Some(l2))?;
    let l0 = write_leaf_owned(&mut pager, &Vec::new(), Some(l1))?;
    let root = write_internal(
        &mut pager,
        &[(l0, b"a"), (l1, b"c"), (l2, b"d"), (l3, b"f")],
        l4,
    )?;
    let tree = BTree::open(&pager, root)?;

    let entries = strs(&[("a", "A"), ("b", "B"), ("d", "D"), ("e", "E")]);
    check_all_ranges(tree, &pager, &entries)
}

#[test]
fn empty_leaves_are_skipped_across_parents() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let l3 = write_leaf_owned(&mut pager, &strs(&[("d", "D"), ("e", "E")]), None)?;
    let l2 = write_leaf_owned(&mut pager, &Vec::new(), Some(l3))?;
    let l1 = write_leaf_owned(&mut pager, &strs(&[("a", "A"), ("b", "B")]), Some(l2))?;
    let l0 = write_leaf_owned(&mut pager, &Vec::new(), Some(l1))?;
    let left = write_internal(&mut pager, &[(l0, b"a")], l1)?;
    let right = write_internal(&mut pager, &[(l2, b"d")], l3)?;
    let root = write_internal(&mut pager, &[(left, b"c")], right)?;
    let tree = BTree::open(&pager, root)?;

    let entries = strs(&[("a", "A"), ("b", "B"), ("d", "D"), ("e", "E")]);
    check_all_ranges(tree, &pager, &entries)
}

// ------------------------------------------------------------- corruption

/// Reads until the cursor fails and returns the error.
fn drain(cursor: &mut Cursor<'_>) -> anyhow::Result<BTreeError> {
    loop {
        match cursor.next_entry() {
            Ok(Some(_)) => {}
            Ok(None) => bail!("the scan ended without an error"),
            Err(error) => return Ok(error),
        }
    }
}

#[test]
fn a_leaf_cycle_is_an_error_not_a_hang() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let a = write_leaf(&mut pager, &[(b"a", b"A")], None)?;
    let b = write_leaf(&mut pager, &[(b"b", b"B")], Some(a))?;
    // Close the cycle a -> b -> a.
    let mut buf = blank();
    pager.read_page(a, &mut buf)?;
    LeafNodeMut::new(&mut buf)?.set_next_leaf(Some(b))?;
    pager.write_page(a, &mut buf)?;
    let tree = BTree::open(&pager, a)?;

    let mut cursor = tree.range(&pager, Unbounded, Unbounded, Direction::Forward)?;
    let error = drain(&mut cursor)?;
    ensure!(
        matches!(error, BTreeError::CorruptTree { reason, .. } if reason == "leaf chain is longer than the file"),
        "unexpected error: {error}"
    );
    ensure!(cursor.next_entry()?.is_none(), "not fused after an error");
    Ok(())
}

#[test]
fn next_leaf_to_an_internal_page_is_an_error() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let other = write_leaf(&mut pager, &[(b"z", b"Z")], None)?;
    let internal = write_internal(&mut pager, &[], other)
        .or_else(|_| write_internal(&mut pager, &[(other, b"y")], other))?;
    let a = write_leaf(&mut pager, &[(b"a", b"A")], Some(internal))?;
    let tree = BTree::open(&pager, a)?;

    let mut cursor = tree.range(&pager, Unbounded, Unbounded, Direction::Forward)?;
    let error = drain(&mut cursor)?;
    ensure!(
        matches!(error, BTreeError::Node { .. }),
        "unexpected error: {error}"
    );
    ensure!(cursor.next_entry()?.is_none(), "not fused after an error");
    Ok(())
}

// ----------------------------------------------------------------- reopen

#[test]
fn scans_work_after_closing_and_reopening_the_file() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, entries) = put_tree(&mut pager, 400)?;
    let root = tree.root();
    pager.sync()?;
    drop(pager);

    let reopened = Pager::open(&path)?;
    let tree = BTree::open(&reopened, root)?;
    let model = model_of(&entries);
    let low = entries.get(100).context("entry")?.0.clone();
    let high = entries.get(300).context("entry")?.0.clone();
    for (lower, upper) in bound_pairs(&low, &high) {
        check_range(tree, &reopened, &model, &lower, &upper)?;
    }
    let first = entries.first().context("entries")?;
    ensure_value(get(tree, &reopened, &first.0)?.as_deref(), &first.1, "get")?;
    Ok(())
}

#[test]
fn a_bulk_loaded_tree_scans_like_a_put_tree() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let entries = numbered_entries(500);
    let root = bulk_load(&mut pager, &entries)?;
    let tree = BTree::open(&pager, root)?;
    let model = model_of(&entries);
    let low = entries.get(120).context("entry")?.0.clone();
    for (lower, upper) in bound_pairs(&low, b"key-00800") {
        check_range(tree, &pager, &model, &lower, &upper)?;
    }
    if leaf_chain(&pager, tree)?.len() < 2 {
        bail!("expected several leaves");
    }
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

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| {
            u8::try_from(i.wrapping_mul(31).wrapping_add(usize::from(seed)) % 251).unwrap_or(0)
        })
        .collect()
}

fn run_model(
    keys: &[Vec<u8>],
    ops: &[(usize, usize, u8)],
    bounds: &[(Spec, Spec)],
) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    for &(key_index, len, seed) in ops {
        let key = keys
            .get(key_index.checked_rem(keys.len()).context("no keys")?)
            .context("key")?;
        let value = pattern(len, seed);
        tree.put(&mut pager, key, &value)?;
        model.insert(key.clone(), value);
    }

    check_range(tree, &pager, &model, &Unbounded, &Unbounded)?;
    for (lower, upper) in bounds {
        check_range(tree, &pager, &model, &make_bound(lower), &make_bound(upper))?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn prop_ranges_match_the_model(
        keys in prop::collection::vec(
            prop::collection::vec(prop::sample::select(vec![b'a', b'b', b'c']), 0..6),
            1..=40,
        ),
        ops in prop::collection::vec(
            (
                any::<usize>(),
                prop_oneof![0usize..120, 100usize..1200, 480usize..500],
                any::<u8>(),
            ),
            1..120,
        ),
        bounds in prop::collection::vec((bound_strategy(), bound_strategy()), 1..8),
    ) {
        run_model(&keys, &ops, &bounds).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
