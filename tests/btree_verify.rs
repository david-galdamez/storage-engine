//! Tests for `verify_tree` and `verify_file` (Phase 2, step 12a). Page size is
//! 512 bytes.
//!
//! Valid trees pass and report sensible counts: an empty root leaf, a bulk
//! loaded tree of three levels, a tree built by sequential puts (thin right
//! leaves are not a violation), a tree with overflow values and a tree after
//! deleting every key.
//!
//! Corrupt trees are hand-built or damaged page by page, and each one must be
//! reported with the right `Violation`: leaf links, leaf depth, key bounds,
//! empty nodes, a page reached twice (a node, a chain, a node of another
//! tree), a non-canonical value, overflow chains with the wrong shape or
//! length, and a leaked page.
//!
//! `KeyTooLarge` has no test: every write path of the public API rejects a
//! key longer than `max_key_len`, so a tree with such a key cannot be built
//! without patching raw cell bytes.

#[path = "support/build.rs"]
mod build;
#[path = "support/db.rs"]
mod db;
#[path = "support/shape.rs"]
mod shape;

use anyhow::{Context, ensure};
use build::{bulk_load, write_internal, write_internal_at, write_leaf};
use db::{new_pager, temp_db};
use shape::{blank, height, numbered_entries};
use storage_engine_rs::storage::btree::error::{BTreeError, NodeError, Violation};
use storage_engine_rs::storage::btree::leaf::{LeafNode, LeafNodeMut};
use storage_engine_rs::storage::btree::node::ValueRef;
use storage_engine_rs::storage::btree::overflow::{OverflowPage, OverflowPageMut};
use storage_engine_rs::storage::btree::{BTree, verify_file, verify_tree};
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::page::PageType;
use storage_engine_rs::storage::pager::types::PageId;

/// Asserts that `result` is an `Invariant` error and returns its violation.
fn violation<T>(result: Result<T, BTreeError>) -> anyhow::Result<Violation> {
    match result {
        Err(BTreeError::Invariant { violation, .. }) => Ok(violation),
        Err(other) => Err(other.into()),
        Ok(_) => Err(anyhow::anyhow!("expected an invariant violation, got Ok")),
    }
}

/// Reads leaf `id`, applies `edit` to it and writes it back.
fn edit_leaf(
    pager: &mut Pager,
    id: PageId,
    edit: impl FnOnce(&mut LeafNodeMut<'_>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut buf = blank();
    pager.read_page(id, &mut buf)?;
    edit(&mut LeafNodeMut::new(&mut buf)?)?;
    pager.write_page(id, &mut buf)?;
    Ok(())
}

fn set_next(pager: &mut Pager, id: PageId, next: Option<PageId>) -> anyhow::Result<()> {
    edit_leaf(pager, id, |leaf| Ok(leaf.set_next_leaf(next)?))
}

/// Two leaves under one root, linked correctly. Returns `(tree, left, right)`.
fn two_leaves(pager: &mut Pager) -> anyhow::Result<(BTree, PageId, PageId)> {
    let right = write_leaf(pager, &[(b"n", b"1"), (b"o", b"2")], None)?;
    let left = write_leaf(pager, &[(b"a", b"1"), (b"b", b"2")], Some(right))?;
    let root = write_internal(pager, &[(left, b"m")], right)?;
    Ok((BTree::open(pager, root)?, left, right))
}

/// The `(len, first_page)` of the overflow cell at `index` of leaf `id`.
fn chain_of(pager: &Pager, id: PageId, index: usize) -> anyhow::Result<(u32, PageId)> {
    let mut buf = blank();
    pager.read_page(id, &mut buf)?;
    match LeafNode::new(&buf)?.cell(index)?.value {
        ValueRef::Overflow { len, first_page } => Ok((len, first_page)),
        ValueRef::Inline(_) => anyhow::bail!("cell {index} is inline"),
    }
}

/// A tree with one key and a value of 1000 bytes (3 overflow pages). Returns
/// the tree and the pages of the chain.
fn one_chain(pager: &mut Pager) -> anyhow::Result<(BTree, Vec<PageId>)> {
    let tree = BTree::create(pager)?;
    tree.put(pager, b"k", &[7u8; 1000])?;
    let (_, mut current) = chain_of(pager, tree.root(), 0)?;
    let mut chain = vec![current];
    loop {
        let mut buf = blank();
        pager.read_page(current, &mut buf)?;
        match OverflowPage::new(&buf)?.next()? {
            Some(next) => {
                chain.push(next);
                current = next;
            }
            None => return Ok((tree, chain)),
        }
    }
}

#[test]
fn empty_root_leaf_is_valid() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let stats = verify_tree(&pager, tree)?;
    ensure!(
        (
            stats.height,
            stats.entries,
            stats.node_pages,
            stats.overflow_pages
        ) == (1, 0, 1, 0),
        "{stats:?}"
    );
    ensure!(verify_file(&pager, &[tree])? == stats, "file stats differ");
    Ok(())
}

#[test]
fn bulk_loaded_three_level_tree_is_valid() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let root = bulk_load(&mut pager, &numbered_entries(300))?;
    let tree = BTree::open(&pager, root)?;
    let stats = verify_tree(&pager, tree)?;
    ensure!(height(&pager, root)? >= 3, "tree is too short");
    ensure!(stats.height == height(&pager, root)?, "{stats:?}");
    ensure!(
        stats.entries == 300 && stats.overflow_pages == 0,
        "{stats:?}"
    );
    ensure!(
        stats.node_pages == usize::try_from(pager.page_count())? - 1,
        "{stats:?}"
    );
    verify_file(&pager, &[tree])?;
    Ok(())
}

#[test]
fn tree_built_by_sequential_puts_is_valid() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    for i in 0..500u32 {
        tree.put(&mut pager, format!("key-{i:05}").as_bytes(), b"value")?;
    }
    let stats = verify_file(&pager, &[tree])?;
    ensure!(stats.entries == 500 && stats.height >= 2, "{stats:?}");
    Ok(())
}

#[test]
fn tree_with_overflow_values_is_valid() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    for i in 0..20u8 {
        let value = vec![i; usize::from(i).saturating_mul(150).saturating_add(200)];
        tree.put(&mut pager, format!("key-{i:02}").as_bytes(), &value)?;
    }
    let stats = verify_file(&pager, &[tree])?;
    ensure!(
        stats.entries == 20 && stats.overflow_pages > 20,
        "{stats:?}"
    );
    Ok(())
}

#[test]
fn tree_after_delete_all_is_valid() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let keys: Vec<Vec<u8>> = (0..300u32)
        .map(|i| format!("key-{i:05}").into_bytes())
        .collect();
    for key in &keys {
        tree.put(&mut pager, key, &[1u8; 40])?;
    }
    for key in &keys {
        tree.delete(&mut pager, key)?;
    }
    let stats = verify_file(&pager, &[tree])?;
    ensure!(
        (stats.height, stats.entries, stats.node_pages) == (1, 0, 1),
        "{stats:?}"
    );
    Ok(())
}

#[test]
fn unlinked_leaf_is_a_leaf_link_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, left, right) = two_leaves(&mut pager)?;
    verify_tree(&pager, tree)?;
    set_next(&mut pager, left, None)?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(
        found
            == Violation::LeafLink {
                found: None,
                expected: Some(right)
            },
        "{found:?}"
    );
    Ok(())
}

#[test]
fn last_leaf_with_a_next_is_a_leaf_link_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, left, right) = two_leaves(&mut pager)?;
    set_next(&mut pager, right, Some(left))?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(
        found
            == Violation::LeafLink {
                found: Some(left),
                expected: None
            },
        "{found:?}"
    );
    Ok(())
}

#[test]
fn leaf_one_level_short_is_a_leaf_depth_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let l3 = write_leaf(&mut pager, &[(b"x", b"1")], None)?;
    let l2 = write_leaf(&mut pager, &[(b"n", b"1")], Some(l3))?;
    let l1 = write_leaf(&mut pager, &[(b"a", b"1")], Some(l2))?;
    let inner = write_internal(&mut pager, &[(l2, b"w")], l3)?;
    let root = write_internal(&mut pager, &[(l1, b"m")], inner)?;
    let tree = BTree::open(&pager, root)?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(
        found
            == Violation::LeafDepth {
                depth: 2,
                expected: 1
            },
        "{found:?}"
    );
    Ok(())
}

#[test]
fn separator_above_the_right_keys_is_a_bounds_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let right = write_leaf(&mut pager, &[(b"n", b"1"), (b"o", b"2")], None)?;
    let left = write_leaf(&mut pager, &[(b"a", b"1")], Some(right))?;
    let root = write_internal(&mut pager, &[(left, b"z")], right)?;
    let tree = BTree::open(&pager, root)?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(found == Violation::KeyOutOfBounds { index: 0 }, "{found:?}");
    Ok(())
}

#[test]
fn empty_non_root_leaf_is_a_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let right = write_leaf(&mut pager, &[], None)?;
    let left = write_leaf(&mut pager, &[(b"a", b"1")], Some(right))?;
    let root = write_internal(&mut pager, &[(left, b"m")], right)?;
    let tree = BTree::open(&pager, root)?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(found == Violation::EmptyLeaf, "{found:?}");
    Ok(())
}

#[test]
fn internal_node_without_cells_is_a_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let leaf = write_leaf(&mut pager, &[(b"a", b"1")], None)?;
    let root = write_internal(&mut pager, &[], leaf)?;
    let tree = BTree::open(&pager, root)?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(found == Violation::EmptyInternal, "{found:?}");
    Ok(())
}

#[test]
fn a_tree_deeper_than_the_maximum_height_is_too_deep() -> anyhow::Result<()> {
    // `MAX_TREE_HEIGHT` (src/storage/btree/tree.rs) is 33 and not public, so
    // build 40 levels, comfortably past it.
    const LEVELS: usize = 40;
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let mut child = write_leaf(&mut pager, &[(&[0], b"1")], None)?;
    // Separators grow outward (2, 4, ...), so every subtree stays inside the
    // bounds of its ancestors. The deep chain is the leftmost child, so the
    // walk descends it first.
    for sep in (2u8..).step_by(2).take(LEVELS) {
        let right = write_leaf(&mut pager, &[(&[sep], b"2")], None)?;
        child = write_internal(&mut pager, &[(child, &[sep])], right)?;
    }
    let tree = BTree::open(&pager, child)?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(found == Violation::TooDeep, "{found:?}");
    Ok(())
}

#[test]
fn two_parents_of_one_child_is_a_reached_twice_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let leaf = write_leaf(&mut pager, &[(b"a", b"1")], None)?;
    let root = write_internal(&mut pager, &[(leaf, b"m")], leaf)?;
    let tree = BTree::open(&pager, root)?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(found == Violation::PageReachedTwice, "{found:?}");
    Ok(())
}

#[test]
fn two_cells_on_one_chain_is_a_reached_twice_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    tree.put(&mut pager, b"a", &[1u8; 1000])?;
    tree.put(&mut pager, b"b", &[2u8; 1000])?;
    let (len, first_page) = chain_of(&pager, tree.root(), 0)?;
    edit_leaf(&mut pager, tree.root(), |leaf| {
        leaf.remove(1)?;
        Ok(leaf.insert(1, b"b", ValueRef::Overflow { len, first_page })?)
    })?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(found == Violation::PageReachedTwice, "{found:?}");
    Ok(())
}

#[test]
fn short_last_chain_page_is_an_overflow_length_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, chain) = one_chain(&mut pager)?;
    verify_tree(&pager, tree)?;
    let last = *chain.last().context("chain")?;
    let mut buf = blank();
    OverflowPageMut::init(&mut buf, None, &[7u8; 10])?;
    pager.write_page(last, &mut buf)?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(
        found
            == Violation::OverflowLength {
                index: 0,
                expected: 1000,
                found: 488 + 488 + 10
            },
        "{found:?}"
    );
    Ok(())
}

#[test]
fn short_middle_chain_page_is_an_overflow_not_full_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, chain) = one_chain(&mut pager)?;
    let (first, second) = (
        *chain.first().context("chain")?,
        *chain.get(1).context("chain")?,
    );
    let mut buf = blank();
    OverflowPageMut::init(&mut buf, Some(second), &[7u8; 10])?;
    pager.write_page(first, &mut buf)?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(found == Violation::OverflowNotFull, "{found:?}");
    Ok(())
}

#[test]
fn overflow_cell_with_a_small_value_is_a_non_canonical_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    edit_leaf(&mut pager, tree.root(), |leaf| {
        let first_page = PageId::new(1);
        Ok(leaf.insert(0, b"k", ValueRef::Overflow { len: 5, first_page })?)
    })?;
    let found = violation(verify_tree(&pager, tree))?;
    ensure!(
        found == Violation::NonCanonicalValue { index: 0 },
        "{found:?}"
    );
    Ok(())
}

#[test]
fn leaked_page_is_a_page_accounting_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    verify_file(&pager, &[tree])?;
    pager.allocate_page()?;
    verify_tree(&pager, tree)?;
    let found = violation(verify_file(&pager, &[tree]))?;
    ensure!(
        found
            == Violation::PageAccounting {
                reached: 1,
                free: 0,
                total: 2
            },
        "{found:?}"
    );
    Ok(())
}

#[test]
fn trees_sharing_a_child_are_a_reached_twice_violation() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let shared = write_leaf(&mut pager, &[(b"x", b"1")], None)?;
    let l1 = write_leaf(&mut pager, &[(b"a", b"1")], Some(shared))?;
    let l2 = write_leaf(&mut pager, &[(b"b", b"1")], Some(shared))?;
    let r1 = write_internal(&mut pager, &[(l1, b"m")], shared)?;
    let r2 = write_internal(&mut pager, &[(l2, b"m")], shared)?;
    let (t1, t2) = (BTree::open(&pager, r1)?, BTree::open(&pager, r2)?);
    verify_tree(&pager, t1)?;
    verify_tree(&pager, t2)?;
    let found = violation(verify_file(&pager, &[t1, t2]))?;
    ensure!(found == Violation::PageReachedTwice, "{found:?}");
    Ok(())
}

#[test]
fn internal_node_with_keys_out_of_order_is_a_node_error() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let a = write_leaf(&mut pager, &[(b"a", b"1")], None)?;
    let b = write_leaf(&mut pager, &[(b"m", b"1")], None)?;
    let c = write_leaf(&mut pager, &[(b"z", b"1")], None)?;
    let root = write_internal(&mut pager, &[(a, b"z"), (b, b"m")], c)?;
    let tree = BTree::open(&pager, root)?;
    match verify_tree(&pager, tree) {
        Err(BTreeError::Node {
            page,
            source: NodeError::CorruptNode { .. },
        }) if page == root => Ok(()),
        other => anyhow::bail!("unexpected result: {other:?}"),
    }
}

#[test]
fn child_that_is_not_a_node_is_reported_with_its_found_type() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let leaf = write_leaf(&mut pager, &[(b"a", b"1")], None)?;
    // Allocate the root first so that `write_internal` does not reuse the freed page.
    let root = pager.allocate_page()?;
    let freed = pager.allocate_page()?;
    pager.free_page(freed)?;
    write_internal_at(&mut pager, root, &[(leaf, b"m")], freed)?;
    let tree = BTree::open(&pager, root)?;
    match verify_tree(&pager, tree) {
        Err(BTreeError::Invariant {
            page,
            violation: Violation::NotANode {
                found: PageType::Free,
            },
        }) if page == freed => Ok(()),
        other => anyhow::bail!("unexpected result: {other:?}"),
    }
}
