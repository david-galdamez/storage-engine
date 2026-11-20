//! Tests for `BTree::get` (Phase 2, step 4).
//!
//! Trees are built by hand with the helpers in `btree_support`.
//!
//! The tests check:
//!
//! - an empty tree returns `false` and leaves `out` untouched;
//! - a single leaf: hit, misses below, between and above, the empty key, the
//!   empty value and the largest inline value;
//! - `out` is replaced (not appended) on a hit and untouched on a miss;
//! - a two-level tree: a key equal to a separator goes right, keys below it go
//!   left, keys above the last separator go to the rightmost child;
//! - a three-level tree from the bulk loader returns every value;
//! - an overflow cell with a dangling first page gives a pager error and
//!   leaves `out` empty (chains are tested in `btree_overflow.rs`);
//! - corrupt structure: a child that is not a node, a child that points to
//!   itself (no hang) and a child past the end of the file;
//! - `get` still works after closing and reopening the file;
//! - a property test against a `BTreeMap` model.
//!
//! Page size is 512 bytes.

mod btree_support;

use std::collections::BTreeMap;

use anyhow::{Context, bail, ensure};
use btree_support::{
    blank, bulk_load, ensure_missing, ensure_value, get, height, new_pager, numbered_entries,
    page_size, temp_db, write_internal, write_internal_at, write_leaf,
};
use proptest::prelude::*;
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::btree::error::BTreeError;
use storage_engine_rs::storage::btree::leaf::LeafNodeMut;
use storage_engine_rs::storage::btree::node::{ValueRef, max_node_cell_len};
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::error::PagerError;
use storage_engine_rs::storage::pager::slotted::SlottedPageMut;
use storage_engine_rs::storage::pager::types::PageId;

fn get_err(tree: BTree, pager: &Pager, key: &[u8]) -> anyhow::Result<BTreeError> {
    let mut out = b"untouched".to_vec();
    match tree.get(pager, key, &mut out) {
        Ok(found) => bail!("get should fail, returned {found}"),
        Err(e) => {
            ensure!(out == b"untouched", "out changed on error");
            Ok(e)
        }
    }
}

#[test]
fn empty_tree_returns_false_and_leaves_out_untouched() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let mut out = b"keep".to_vec();

    let found = tree.get(&pager, b"anything", &mut out)?;
    ensure!(!found, "empty tree returned true");
    ensure!(out == b"keep", "out was changed");

    let found = tree.get(&pager, b"", &mut out)?;
    ensure!(!found, "empty key found in empty tree");
    ensure!(out == b"keep", "out was changed");
    Ok(())
}

#[test]
fn single_leaf_hits_and_misses() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let id = write_leaf(
        &mut pager,
        &[(b"b", b"1"), (b"d", b"2"), (b"f", b"3")],
        None,
    )?;
    let tree = BTree::open(&pager, id)?;

    ensure_value(get(tree, &pager, b"b")?.as_deref(), b"1", "b")?;
    ensure_value(get(tree, &pager, b"d")?.as_deref(), b"2", "d")?;
    ensure_value(get(tree, &pager, b"f")?.as_deref(), b"3", "f")?;
    ensure_missing(get(tree, &pager, b"a")?.as_deref(), "below")?;
    ensure_missing(get(tree, &pager, b"c")?.as_deref(), "between b and d")?;
    ensure_missing(get(tree, &pager, b"e")?.as_deref(), "between d and f")?;
    ensure_missing(get(tree, &pager, b"g")?.as_deref(), "above")?;
    ensure_missing(get(tree, &pager, b"bb")?.as_deref(), "prefix extension")?;
    ensure_missing(get(tree, &pager, b"")?.as_deref(), "empty key")?;
    Ok(())
}

#[test]
fn empty_key_and_empty_value() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let id = write_leaf(&mut pager, &[(b"", b"root"), (b"k", b"")], None)?;
    let tree = BTree::open(&pager, id)?;

    ensure_value(get(tree, &pager, b"")?.as_deref(), b"root", "empty key")?;

    let mut out = b"old".to_vec();
    ensure!(
        tree.get(&pager, b"k", &mut out)?,
        "empty value must be found"
    );
    ensure!(out.is_empty(), "out should be empty, got {out:?}");
    Ok(())
}

#[test]
fn value_at_the_inline_maximum() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let key = b"key";
    let max_value = max_node_cell_len(page_size()?)
        .checked_sub(3)
        .and_then(|n| n.checked_sub(key.len()))
        .context("limit underflow")?;
    let value = vec![0xAB; max_value];
    let id = write_leaf(&mut pager, &[(key, &value)], None)?;
    let tree = BTree::open(&pager, id)?;

    ensure_value(
        get(tree, &pager, key)?.as_deref(),
        &value,
        "max inline value",
    )
}

#[test]
fn out_is_replaced_on_hit_and_untouched_on_miss() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let id = write_leaf(&mut pager, &[(b"a", b"short"), (b"b", b"x")], None)?;
    let tree = BTree::open(&pager, id)?;

    let mut out = b"a much longer previous content".to_vec();
    ensure!(tree.get(&pager, b"a", &mut out)?, "a not found");
    ensure!(out == b"short", "out was appended or not replaced: {out:?}");

    ensure!(!tree.get(&pager, b"zzz", &mut out)?, "zzz found");
    ensure!(out == b"short", "out changed on a miss: {out:?}");

    ensure!(tree.get(&pager, b"b", &mut out)?, "b not found");
    ensure!(out == b"x", "out not replaced by a shorter value: {out:?}");
    Ok(())
}

#[test]
fn two_level_tree_routes_by_separator() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let l2 = write_leaf(&mut pager, &[(b"x", b"X"), (b"y", b"Y")], None)?;
    let l1 = write_leaf(&mut pager, &[(b"m", b"M"), (b"n", b"N")], Some(l2))?;
    let l0 = write_leaf(&mut pager, &[(b"a", b"A"), (b"b", b"B")], Some(l1))?;
    let root = write_internal(&mut pager, &[(l0, b"m"), (l1, b"x")], l2)?;
    let tree = BTree::open(&pager, root)?;

    // Equal to a separator: found in the right child.
    ensure_value(get(tree, &pager, b"m")?.as_deref(), b"M", "m (separator)")?;
    ensure_value(get(tree, &pager, b"x")?.as_deref(), b"X", "x (separator)")?;
    // Just below a separator: left child.
    ensure_value(get(tree, &pager, b"b")?.as_deref(), b"B", "b")?;
    ensure_value(get(tree, &pager, b"n")?.as_deref(), b"N", "n")?;
    ensure_missing(get(tree, &pager, b"l")?.as_deref(), "just below m")?;
    ensure_missing(get(tree, &pager, b"w")?.as_deref(), "just below x")?;
    // Above the last separator: rightmost child.
    ensure_value(get(tree, &pager, b"y")?.as_deref(), b"Y", "y")?;
    ensure_missing(get(tree, &pager, b"z")?.as_deref(), "above all keys")?;
    ensure_missing(get(tree, &pager, b"")?.as_deref(), "empty key")?;
    Ok(())
}

#[test]
fn three_level_tree_finds_every_key() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let entries = numbered_entries(400);
    let root = bulk_load(&mut pager, &entries)?;
    ensure!(
        height(&pager, root)? >= 3,
        "tree has only {} levels",
        height(&pager, root)?
    );
    let tree = BTree::open(&pager, root)?;

    for (key, value) in &entries {
        ensure_value(get(tree, &pager, key)?.as_deref(), value, "present key")?;
    }
    // Odd numbers are absent, and so are keys outside the range.
    for i in 0..400usize {
        let absent = format!("key-{:05}", i.saturating_mul(2).saturating_add(1));
        ensure_missing(
            get(tree, &pager, absent.as_bytes())?.as_deref(),
            "absent key",
        )?;
    }
    ensure_missing(get(tree, &pager, b"")?.as_deref(), "below all keys")?;
    ensure_missing(get(tree, &pager, b"zzz")?.as_deref(), "above all keys")?;
    Ok(())
}

#[test]
fn overflow_cell_with_a_dangling_page_is_a_pager_error() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let id = pager.allocate_page()?;
    let mut buf = blank();
    LeafNodeMut::init(&mut buf)?;
    LeafNodeMut::new(&mut buf)?.insert(
        0,
        b"big",
        ValueRef::Overflow {
            len: 5000,
            first_page: PageId::new(99),
        },
    )?;
    pager.write_page(id, &mut buf)?;
    let tree = BTree::open(&pager, id)?;

    let mut out = b"old".to_vec();
    match get_err_into(tree, &pager, b"big", &mut out)? {
        BTreeError::Pager(PagerError::PageOutOfBounds { page, .. }) => {
            ensure!(page == PageId::new(99), "wrong page: {page}");
        }
        other => bail!("expected a pager error, got {other:?}"),
    }
    ensure!(out.is_empty(), "out should be empty after a failed read");
    // A miss does not touch the overflow cell.
    ensure_missing(get(tree, &pager, b"small")?.as_deref(), "other key")
}

fn get_err_into(
    tree: BTree,
    pager: &Pager,
    key: &[u8],
    out: &mut Vec<u8>,
) -> anyhow::Result<BTreeError> {
    match tree.get(pager, key, out) {
        Ok(found) => bail!("get should fail, returned {found}"),
        Err(e) => Ok(e),
    }
}

fn ensure_corrupt_tree(err: &BTreeError, page: PageId, reason: &str) -> anyhow::Result<()> {
    match err {
        BTreeError::CorruptTree { page: p, reason: r } => {
            ensure!(*p == page, "wrong page: {}", p.get());
            ensure!(*r == reason, "wrong reason: {r}");
            Ok(())
        }
        other => bail!("expected CorruptTree, got {other:?}"),
    }
}

#[test]
fn child_that_is_a_free_page_is_corrupt() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let leaf = write_leaf(&mut pager, &[(b"z", b"Z")], None)?;
    // Allocate the root first: a freed page would be reused by the next
    // allocation.
    let root = pager.allocate_page()?;
    let freed = pager.allocate_page()?;
    pager.free_page(freed)?;
    write_internal_at(&mut pager, root, &[(freed, b"m")], leaf)?;
    let tree = BTree::open(&pager, root)?;

    let err = get_err(tree, &pager, b"a")?;
    ensure_corrupt_tree(&err, freed, "child is not a B+Tree node")?;
    // The other side of the separator is still readable.
    ensure_value(get(tree, &pager, b"z")?.as_deref(), b"Z", "z")
}

#[test]
fn child_that_is_a_slotted_page_is_corrupt() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let slotted = pager.allocate_page()?;
    let mut buf = blank();
    SlottedPageMut::init(&mut buf)?;
    pager.write_page(slotted, &mut buf)?;
    let leaf = write_leaf(&mut pager, &[(b"z", b"Z")], None)?;
    let root = write_internal(&mut pager, &[(leaf, b"m")], slotted)?;
    let tree = BTree::open(&pager, root)?;

    let err = get_err(tree, &pager, b"q")?;
    ensure_corrupt_tree(&err, slotted, "child is not a B+Tree node")
}

#[test]
fn root_overwritten_by_a_non_node_is_corrupt() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let mut buf = blank();
    SlottedPageMut::init(&mut buf)?;
    pager.write_page(tree.root(), &mut buf)?;

    let err = get_err(tree, &pager, b"a")?;

    ensure_corrupt_tree(&err, tree.root(), "root is not a B+Tree node")
}

#[test]
fn child_pointing_to_itself_hits_the_height_guard() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let id = pager.allocate_page()?;
    write_internal_at(&mut pager, id, &[(id, b"m")], id)?;
    let tree = BTree::open(&pager, id)?;

    for key in [&b"a"[..], b"m", b"z"] {
        let err = get_err(tree, &pager, key)?;
        ensure_corrupt_tree(&err, id, "tree is deeper than the maximum height")?;
    }
    Ok(())
}

#[test]
fn child_past_the_end_of_the_file_is_a_pager_error() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let leaf = write_leaf(&mut pager, &[(b"z", b"Z")], None)?;
    let missing = PageId::new(
        pager
            .page_count()
            .checked_add(5)
            .context("page count overflow")?,
    );
    let root = write_internal(&mut pager, &[(missing, b"m")], leaf)?;
    let tree = BTree::open(&pager, root)?;

    match get_err(tree, &pager, b"a")? {
        BTreeError::Pager(PagerError::PageOutOfBounds { page, .. }) => {
            ensure!(page == missing, "wrong page: {}", page.get());
        }
        other => bail!("expected PageOutOfBounds, got {other:?}"),
    }
    Ok(())
}

#[test]
fn get_works_after_reopening_the_file() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let entries = numbered_entries(120);
    let root = bulk_load(&mut pager, &entries)?;
    pager.sync()?;
    drop(pager);

    let reopened = Pager::open(&path)?;
    let tree = BTree::open(&reopened, root)?;
    for (key, value) in &entries {
        ensure_value(get(tree, &reopened, key)?.as_deref(), value, "after reopen")?;
    }
    ensure_missing(
        get(tree, &reopened, b"key-00001")?.as_deref(),
        "absent after reopen",
    )
}

fn run_model(map: &BTreeMap<Vec<u8>, Vec<u8>>, probes: &[Vec<u8>]) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let entries: Vec<(Vec<u8>, Vec<u8>)> =
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let root = bulk_load(&mut pager, &entries)?;
    let tree = BTree::open(&pager, root)?;

    for (key, value) in map {
        ensure_value(get(tree, &pager, key)?.as_deref(), value, "model key")?;
    }
    for probe in probes {
        let got = get(tree, &pager, probe)?;
        ensure!(
            got.as_ref() == map.get(probe),
            "probe {probe:?}: tree {got:?}, model {:?}",
            map.get(probe)
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_get_matches_btreemap_model(
        map in prop::collection::btree_map(
            prop::collection::vec(any::<u8>(), 0..40),
            prop::collection::vec(any::<u8>(), 0..20),
            1..200,
        ),
        probes in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..40), 0..50),
    ) {
        run_model(&map, &probes).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
