//! Tests for rebalancing after `BTree::delete` (Phase 2, steps 11a and 11b).
//! Page size is 512 bytes: 480 usable bytes, so a non-root node is underfull
//! below 120 used bytes.
//!
//! The tests check, on hand-built trees:
//!
//! - merging an underfull leaf into its left sibling, and (at position 0)
//!   merging the right sibling into it: the next-leaf chain, the parent, and
//!   the freed page being reused by the next puts;
//! - redistributing when the two leaves do not fit in one page: the new
//!   separator is the first key of the new right leaf;
//! - overflow values moving between leaves: chains intact, nothing leaks;
//! - collapsing a root over two leaves: the root page id stays and the tree
//!   opens again;
//! - the work above the parent (step 11b): a merge that empties a non-root
//!   parent merges the parent in turn (the separator is pulled down) and
//!   collapses the root at height 3; an internal rotation when two internal
//!   nodes do not fit in one page; a separator that does not fit in the
//!   parent splits it, even the root (the tree grows during a delete);
//! - an overflow cell that moves between leaves in a redistribution;
//!
//! and on trees built with `put` and `bulk_load`:
//!
//! - deleting every key of a tree of two or three levels in ascending,
//!   descending and scrambled order ends with an empty root leaf with the same
//!   page id and every other page free, and no node is ever left empty;
//! - a property test of mixed puts and deletes against a `BTreeMap`.

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
use std::ops::Bound::Unbounded;

use anyhow::{Context, ensure};
use btree_check::{Entries, check_tree, leaf_chain};
use build::{bulk_load, write_internal, write_leaf};
use db::{new_pager, temp_db};
use leaks::ensure_no_leaks;
use lookup::{ensure_missing, ensure_value, get};
use proptest::prelude::*;
use shape::{blank, height, numbered_entries};
use storage_engine_rs::storage::btree::internal::InternalNode;
use storage_engine_rs::storage::btree::leaf::LeafNode;
use storage_engine_rs::storage::btree::overflow::read_value;
use storage_engine_rs::storage::btree::{BTree, Direction, verify_tree};
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::types::PageId;

/// A small value: a leaf with a few of them is underfull.
const V: &[u8] = b"v";

fn read(pager: &Pager, id: PageId) -> anyhow::Result<Vec<u8>> {
    let mut buf = blank();
    pager.read_page(id, &mut buf)?;
    Ok(buf)
}

fn leaf_keys(pager: &Pager, id: PageId) -> anyhow::Result<Vec<Vec<u8>>> {
    let buf = read(pager, id)?;
    let leaf = LeafNode::new(&buf)?;
    let mut keys = Vec::new();
    for index in 0..leaf.cell_count() {
        keys.push(leaf.key(index)?.to_vec());
    }
    Ok(keys)
}

fn next_leaf(pager: &Pager, id: PageId) -> anyhow::Result<Option<PageId>> {
    let buf = read(pager, id)?;
    Ok(LeafNode::new(&buf)?.next_leaf()?)
}

/// The `(child, key)` cells of an internal node and its rightmost child.
type Cells = (Vec<(PageId, Vec<u8>)>, PageId);

fn cells_of(pager: &Pager, id: PageId) -> anyhow::Result<Cells> {
    let buf = read(pager, id)?;
    let node = InternalNode::new(&buf)?;
    let mut cells = Vec::new();
    for index in 0..node.cell_count() {
        cells.push((node.child_at(index)?, node.key(index)?.to_vec()));
    }
    Ok((cells, node.rightmost_child()?))
}

fn keys(list: &[&[u8]]) -> Vec<Vec<u8>> {
    list.iter().map(|key| key.to_vec()).collect()
}

/// The entries of a leaf-level tree as `check_tree` reports them, for the
/// given keys all holding [`V`].
fn small_entries(list: &[&[u8]]) -> Entries {
    list.iter().map(|key| (key.to_vec(), V.to_vec())).collect()
}

/// A height-2 tree with a root of two cells: leaves `[a b c]`, `[d e]`,
/// `[h i]`. Returns the tree and the three leaf ids.
fn three_leaves(pager: &mut Pager) -> anyhow::Result<(BTree, [PageId; 3])> {
    let l3 = write_leaf(pager, &[(b"h", V), (b"i", V)], None)?;
    let l2 = write_leaf(pager, &[(b"d", V), (b"e", V)], Some(l3))?;
    let l1 = write_leaf(pager, &[(b"a", V), (b"b", V), (b"c", V)], Some(l2))?;
    let root = write_internal(pager, &[(l1, b"d"), (l2, b"h")], l3)?;
    Ok((BTree::open(pager, root)?, [l1, l2, l3]))
}

/// Puts keys `c0`, `c1`... with large inline values until the freed page is
/// taken by a split, and checks that no page was added to the file.
fn refill_until_the_free_page_is_used(pager: &mut Pager, tree: BTree) -> anyhow::Result<()> {
    let count = pager.page_count();
    ensure!(pager.free_page_count() == 1, "expected one free page");
    let value = [5u8; 100];
    for i in 0..20 {
        tree.put(pager, format!("c{i}").as_bytes(), &value)?;
        if pager.free_page_count() == 0 {
            break;
        }
    }
    ensure!(pager.free_page_count() == 0, "the free page was not reused");
    ensure!(pager.page_count() == count, "the file grew");
    ensure_no_leaks(pager, tree)?;
    Ok(())
}

// ------------------------------------------------------------------ merge

#[test]
fn merge_with_the_left_sibling() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, [l1, l2, l3]) = three_leaves(&mut pager)?;

    ensure!(tree.delete(&mut pager, b"e")?, "not deleted");

    ensure!(leaf_keys(&pager, l1)? == keys(&[b"a", b"b", b"c", b"d"]));
    ensure!(next_leaf(&pager, l1)? == Some(l3), "left leaf must skip l2");
    ensure!(leaf_chain(&pager, tree)? == [l1, l3]);
    let (cells, rightmost) = cells_of(&pager, tree.root())?;
    ensure!(cells == [(l1, b"h".to_vec())], "parent: {cells:?}");
    ensure!(rightmost == l3);
    ensure!(pager.free_page_count() == 1, "l2 {l2} must be free");
    ensure!(
        check_tree(&pager, tree)? == small_entries(&[b"a", b"b", b"c", b"d", b"h", b"i"]),
        "entries"
    );
    ensure_no_leaks(&pager, tree)?;
    refill_until_the_free_page_is_used(&mut pager, tree)
}

#[test]
fn merge_with_the_right_sibling_at_position_zero() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let (tree, [l1, l2, l3]) = three_leaves(&mut pager)?;
    // `[a b c]` is underfull after the delete (18 bytes) and has no left
    // sibling, so `[d e]` is merged into it.
    ensure!(tree.delete(&mut pager, b"a")?);

    ensure!(leaf_keys(&pager, l1)? == keys(&[b"b", b"c", b"d", b"e"]));
    ensure!(next_leaf(&pager, l1)? == Some(l3));
    ensure!(leaf_chain(&pager, tree)? == [l1, l3]);
    let (cells, rightmost) = cells_of(&pager, tree.root())?;
    ensure!(cells == [(l1, b"h".to_vec())], "parent: {cells:?}");
    ensure!(rightmost == l3);
    ensure!(pager.free_page_count() == 1, "l2 {l2} must be free");
    ensure_no_leaks(&pager, tree)?;
    refill_until_the_free_page_is_used(&mut pager, tree)
}

#[test]
fn a_leaf_above_the_threshold_is_not_rebalanced() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let big = [3u8; 60];
    // Cells of 3 + 1 + 60 + 4 = 68 bytes: 136 used after the delete, above 120.
    let l2 = write_leaf(
        &mut pager,
        &[(b"m", &big), (b"n", &big), (b"o", &big)],
        None,
    )?;
    let l1 = write_leaf(
        &mut pager,
        &[(b"a", &big), (b"b", &big), (b"c", &big)],
        Some(l2),
    )?;
    let root = write_internal(&mut pager, &[(l1, b"m")], l2)?;
    let tree = BTree::open(&pager, root)?;

    ensure!(tree.delete(&mut pager, b"c")?);

    ensure!(
        leaf_keys(&pager, l1)? == keys(&[b"a", b"b"]),
        "136 bytes used"
    );
    ensure!(pager.free_page_count() == 0);
    ensure!(leaf_chain(&pager, tree)? == [l1, l2]);
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

// ----------------------------------------------------------- redistribute

#[test]
fn redistribute_when_the_pair_does_not_fit_in_one_page() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    // Four cells of 3 + 1 + 112 + 4 = 120 bytes fill the 480 usable bytes.
    let full = [9u8; 112];
    let l2 = write_leaf(
        &mut pager,
        &[(b"m", &full), (b"n", &full), (b"o", &full), (b"p", &full)],
        None,
    )?;
    let l1 = write_leaf(&mut pager, &[(b"a", V), (b"b", V)], Some(l2))?;
    let root = write_internal(&mut pager, &[(l1, b"m")], l2)?;
    let tree = BTree::open(&pager, root)?;

    ensure!(tree.delete(&mut pager, b"b")?);

    // 9 + 4 * 120 bytes are cut where the left part reaches half: after `n`.
    ensure!(leaf_keys(&pager, l1)? == keys(&[b"a", b"m", b"n"]));
    ensure!(leaf_keys(&pager, l2)? == keys(&[b"o", b"p"]));
    let (cells, rightmost) = cells_of(&pager, root)?;
    ensure!(
        cells == [(l1, b"o".to_vec())],
        "the separator is the first key of the right leaf: {cells:?}"
    );
    ensure!(rightmost == l2);
    ensure!(next_leaf(&pager, l1)? == Some(l2));
    ensure!(next_leaf(&pager, l2)?.is_none());
    ensure!(pager.free_page_count() == 0);
    ensure_value(get(tree, &pager, b"m")?.as_deref(), &full, "m")?;
    ensure_missing(get(tree, &pager, b"b")?.as_deref(), "b")?;
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

#[test]
fn redistribute_with_the_left_sibling_when_the_leaf_is_the_right_one() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let full = [9u8; 112];
    let l2 = write_leaf(&mut pager, &[(b"z", V), (b"zz", V)], None)?;
    let l1 = write_leaf(
        &mut pager,
        &[(b"a", &full), (b"b", &full), (b"c", &full), (b"d", &full)],
        Some(l2),
    )?;
    let root = write_internal(&mut pager, &[(l1, b"z")], l2)?;
    let tree = BTree::open(&pager, root)?;

    ensure!(tree.delete(&mut pager, b"zz")?);

    // The left leaf gives its last cell: 4 * 120 + 9 bytes, cut after `c`.
    ensure!(leaf_keys(&pager, l1)? == keys(&[b"a", b"b", b"c"]));
    ensure!(leaf_keys(&pager, l2)? == keys(&[b"d", b"z"]));
    let (cells, _) = cells_of(&pager, root)?;
    ensure!(cells == [(l1, b"d".to_vec())], "parent: {cells:?}");
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

// --------------------------------------------------------- overflow values

#[test]
fn overflow_values_survive_merges_and_redistributions() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let mut model = BTreeMap::new();
    for i in 0..60usize {
        let len = if i % 3 == 0 { 300 + i } else { 10 + i % 7 };
        let value: Vec<u8> = (0..len)
            .map(|b| u8::try_from(b.wrapping_add(i) % 251).unwrap_or(0))
            .collect();
        let key = format!("key-{i:03}").into_bytes();
        tree.put(&mut pager, &key, &value)?;
        model.insert(key, value);
    }
    let (overflow, _) = ensure_no_leaks(&pager, tree)?;
    ensure!(overflow > 0, "the test needs overflow values");
    let leaves_before = leaf_chain(&pager, tree)?.len();

    // Delete in an order that empties leaves from the inside.
    for step in 0..60usize {
        let i = step.checked_mul(37).map(|n| n % 60).context("overflow")?;
        let key = format!("key-{i:03}").into_bytes();
        ensure!(tree.delete(&mut pager, &key)?, "key {i} was missing");
        model.remove(&key);

        let (_, entries) = ensure_no_leaks(&pager, tree)?;
        let expected: Entries = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        ensure!(entries == expected, "entries differ after deleting {i}");
    }
    ensure!(leaf_chain(&pager, tree)?.len() < leaves_before, "no merge");
    Ok(())
}

// ----------------------------------------------------------- root collapse

#[test]
fn a_merge_that_empties_the_root_collapses_the_tree() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let l2 = write_leaf(&mut pager, &[(b"d", V), (b"e", V)], None)?;
    let l1 = write_leaf(&mut pager, &[(b"a", V), (b"b", V)], Some(l2))?;
    let root = write_internal(&mut pager, &[(l1, b"d")], l2)?;
    let tree = BTree::open(&pager, root)?;
    ensure!(height(&pager, root)? == 2);

    ensure!(tree.delete(&mut pager, b"b")?);

    ensure!(tree.root() == root);
    ensure!(height(&pager, root)? == 1, "the root is a leaf now");
    ensure!(leaf_chain(&pager, tree)? == [root]);
    ensure!(next_leaf(&pager, root)?.is_none());
    ensure!(pager.free_page_count() == 2, "both leaf pages are free");
    ensure!(check_tree(&pager, tree)? == small_entries(&[b"a", b"d", b"e"]));
    ensure_no_leaks(&pager, tree)?;

    // The tree opens again from the file, and reads and writes work.
    pager.sync()?;
    drop(pager);
    let mut pager = Pager::open(&path)?;
    let tree = BTree::open(&pager, root)?;
    ensure_value(get(tree, &pager, b"d")?.as_deref(), V, "d")?;
    ensure_missing(get(tree, &pager, b"b")?.as_deref(), "b")?;
    tree.put(&mut pager, b"f", V)?;
    ensure!(check_tree(&pager, tree)? == small_entries(&[b"a", b"d", b"e", b"f"]));
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

// -------------------------------------------------- work above the parent

/// A height-3 tree: the root over `p1` and `p2`, `p1` over `[a b] [d e] [g h]`
/// (with `lone` set, over `[a b] [d e]` only), `p2` over `[m n] [s t]`.
struct Deep {
    tree: BTree,
    root: PageId,
    leaves: Vec<PageId>,
}

fn deep(pager: &mut Pager, lone: bool) -> anyhow::Result<Deep> {
    let l5 = write_leaf(pager, &[(b"s", V), (b"t", V)], None)?;
    let l4 = write_leaf(pager, &[(b"m", V), (b"n", V)], Some(l5))?;
    let (p1_leaves, p1) = if lone {
        let l2 = write_leaf(pager, &[(b"d", V), (b"e", V)], Some(l4))?;
        let l1 = write_leaf(pager, &[(b"a", V), (b"b", V)], Some(l2))?;
        (vec![l1, l2], write_internal(pager, &[(l1, b"d")], l2)?)
    } else {
        let l3 = write_leaf(pager, &[(b"g", V), (b"h", V)], Some(l4))?;
        let l2 = write_leaf(pager, &[(b"d", V), (b"e", V)], Some(l3))?;
        let l1 = write_leaf(pager, &[(b"a", V), (b"b", V)], Some(l2))?;
        (
            vec![l1, l2, l3],
            write_internal(pager, &[(l1, b"d"), (l2, b"g")], l3)?,
        )
    };
    let p2 = write_internal(pager, &[(l4, b"s")], l5)?;
    let root = write_internal(pager, &[(p1, b"m")], p2)?;
    let mut leaves = p1_leaves;
    leaves.push(l4);
    leaves.push(l5);
    Ok(Deep {
        tree: BTree::open(pager, root)?,
        root,
        leaves,
    })
}

#[test]
fn a_merge_that_empties_a_non_root_parent_merges_it_and_collapses_the_root() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let deep = deep(&mut pager, true)?;
    let [l1, l2, l4, l5]: [PageId; 4] = deep
        .leaves
        .clone()
        .try_into()
        .map_err(|_| anyhow::anyhow!("four leaves"))?;

    // `[a]` is underfull after the delete and merges with `[d e]`. `p1` is left
    // with no cells, so it merges with `p2` (the root key `m` is pulled down),
    // and the root, left with no cells, takes the merged node.
    ensure!(deep.tree.delete(&mut pager, b"b")?);

    ensure!(deep.tree.root() == deep.root);
    ensure!(height(&pager, deep.root)? == 2, "the tree lost one level");
    ensure!(leaf_keys(&pager, l1)? == keys(&[b"a", b"d", b"e"]));
    let (cells, rightmost) = cells_of(&pager, deep.root)?;
    ensure!(
        cells == [(l1, b"m".to_vec()), (l4, b"s".to_vec())],
        "root: {cells:?}"
    );
    ensure!(rightmost == l5);
    ensure!(leaf_chain(&pager, deep.tree)? == [l1, l4, l5]);
    ensure!(pager.free_page_count() == 3, "l2 {l2}, p1 and p2 are free");
    ensure!(
        check_tree(&pager, deep.tree)?
            == small_entries(&[b"a", b"d", b"e", b"m", b"n", b"s", b"t"])
    );
    verify_tree(&pager, deep.tree)?;
    ensure_no_leaks(&pager, deep.tree)?;

    // The tree opens again from the file, and reads and writes work.
    pager.sync()?;
    drop(pager);
    let mut pager = Pager::open(&path)?;
    let tree = BTree::open(&pager, deep.root)?;
    ensure_value(get(tree, &pager, b"n")?.as_deref(), V, "n")?;
    ensure_missing(get(tree, &pager, b"b")?.as_deref(), "b")?;
    tree.put(&mut pager, b"f", V)?;
    ensure!(check_tree(&pager, tree)?.len() == 8);
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

#[test]
fn a_merge_rebalances_an_underfull_parent_up_to_the_root() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let deep = deep(&mut pager, false)?;
    let [l1, l2, l3, l4, l5]: [PageId; 5] = deep
        .leaves
        .clone()
        .try_into()
        .map_err(|_| anyhow::anyhow!("five leaves"))?;

    ensure!(deep.tree.delete(&mut pager, b"e")?);

    // `[d]` merged into `[a b]`. `p1` kept one cell (9 bytes, underfull), so it
    // merged with `p2` and the root collapsed: its page now holds the merged
    // node, with `g`, the pulled down `m` and `s`.
    ensure!(leaf_keys(&pager, l1)? == keys(&[b"a", b"b", b"d"]));
    let (cells, rightmost) = cells_of(&pager, deep.root)?;
    ensure!(
        cells
            == [
                (l1, b"g".to_vec()),
                (l3, b"m".to_vec()),
                (l4, b"s".to_vec())
            ],
        "root: {cells:?}"
    );
    ensure!(rightmost == l5);
    ensure!(leaf_chain(&pager, deep.tree)? == [l1, l3, l4, l5]);
    ensure!(pager.free_page_count() == 3, "l2 {l2}, p1 and p2 are free");
    ensure!(height(&pager, deep.root)? == 2);
    verify_tree(&pager, deep.tree)?;
    ensure_no_leaks(&pager, deep.tree)?;
    Ok(())
}

/// One leaf for each key, holding only that key, linked in order. The last one
/// gets `next`. Returns the ids in key order.
fn leaf_run(
    pager: &mut Pager,
    keys: &[Vec<u8>],
    next: Option<PageId>,
) -> anyhow::Result<Vec<PageId>> {
    let mut ids = Vec::new();
    let mut following = next;
    for key in keys.iter().rev() {
        let id = write_leaf(pager, &[(key.as_slice(), V)], following)?;
        ids.push(id);
        following = Some(id);
    }
    ids.reverse();
    Ok(ids)
}

/// An internal node over `children` (one more than `separators`).
fn internal_over(
    pager: &mut Pager,
    children: &[PageId],
    separators: &[Vec<u8>],
) -> anyhow::Result<PageId> {
    let (last, init) = children.split_last().context("no children")?;
    let cells: Vec<(PageId, &[u8])> = init
        .iter()
        .zip(separators)
        .map(|(child, key)| (*child, key.as_slice()))
        .collect();
    write_internal(pager, &cells, *last)
}

fn short_key(i: usize) -> Vec<u8> {
    format!("k{i:02}").into_bytes()
}

/// A 30 byte key: an internal cell with it uses 38 bytes.
fn wide_key(i: usize) -> Vec<u8> {
    format!("k{i:02}{}", ".".repeat(27)).into_bytes()
}

#[test]
fn an_internal_merge_pulls_the_separator_down() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    // Eight leaves `k00..k07` under three internal nodes with two leaves each
    // (the last one with the other two), under a root with two cells.
    let all: Vec<Vec<u8>> = (0..8).map(short_key).collect();
    let l = leaf_run(&mut pager, &all, None)?;
    let at = |i: usize| l.get(i).copied().context("leaf");
    let p1 = internal_over(&mut pager, &[at(0)?, at(1)?], &[short_key(1)])?;
    let p2 = internal_over(&mut pager, &[at(2)?, at(3)?], &[short_key(3)])?;
    let p3 = internal_over(
        &mut pager,
        &[at(4)?, at(5)?, at(6)?, at(7)?],
        &[short_key(5), short_key(6), short_key(7)],
    )?;
    let root = internal_over(&mut pager, &[p1, p2, p3], &[short_key(2), short_key(4)])?;
    let tree = BTree::open(&pager, root)?;

    // `k00` is alone in its leaf, which merges with `k01`. `p1` has no cells,
    // so it merges with `p2`, and `k02` comes down from the root.
    ensure!(tree.delete(&mut pager, &short_key(0))?);

    let (cells, rightmost) = cells_of(&pager, p1)?;
    ensure!(
        cells == [(at(0)?, short_key(2)), (at(2)?, short_key(3))],
        "p1: {cells:?}"
    );
    ensure!(rightmost == at(3)?);
    let (cells, rightmost) = cells_of(&pager, root)?;
    ensure!(cells == [(p1, short_key(4))], "root: {cells:?}");
    ensure!(rightmost == p3);
    ensure!(pager.free_page_count() == 2, "a leaf and p2 are free");
    ensure!(height(&pager, root)? == 3);
    ensure!(leaf_keys(&pager, at(0)?)? == vec![short_key(1)]);
    ensure!(leaf_chain(&pager, tree)? == [at(0)?, at(2)?, at(3)?, at(4)?, at(5)?, at(6)?, at(7)?]);
    let expected: Entries = (1..8).map(|i| (short_key(i), V.to_vec())).collect();
    ensure!(check_tree(&pager, tree)? == expected);
    verify_tree(&pager, tree)?;
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

#[test]
fn an_internal_rotation_moves_cells_and_children_between_nodes() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    // 16 leaves `k00..k15`. `small` has four of them and `big` twelve: 11 cells of
    // 38 bytes. After the delete `small` has two cells (76 bytes, underfull), and
    // 76 + 418 + 38 bytes do not fit in one page: the pair is rotated.
    let all: Vec<Vec<u8>> = (0..16).map(wide_key).collect();
    let ids = leaf_run(&mut pager, &all, None)?;
    let small_children = ids.get(..4).context("small children")?;
    let big_children = ids.get(4..).context("big children")?;
    let small = internal_over(&mut pager, small_children, all.get(1..4).context("keys")?)?;
    let big = internal_over(&mut pager, big_children, all.get(5..16).context("keys")?)?;
    let root = internal_over(&mut pager, &[small, big], all.get(4..5).context("keys")?)?;
    let tree = BTree::open(&pager, root)?;
    let id = |i: usize| ids.get(i).copied().context("leaf");

    ensure!(tree.delete(&mut pager, &wide_key(0))?);

    // The 14 cells (`k02`..`k15`) are cut after 7: `k09` goes up.
    let (cells, rightmost) = cells_of(&pager, root)?;
    ensure!(cells == [(small, wide_key(9))], "root: {cells:?}");
    ensure!(rightmost == big);
    let (cells, rightmost) = cells_of(&pager, small)?;
    let want_left: Vec<(PageId, Vec<u8>)> =
        [(0, 2), (2, 3), (3, 4), (4, 5), (5, 6), (6, 7), (7, 8)]
            .into_iter()
            .map(|(child, key)| Ok((id(child)?, wide_key(key))))
            .collect::<anyhow::Result<_>>()?;
    ensure!(cells == want_left, "small: {cells:?}");
    ensure!(rightmost == id(8)?);
    let (cells, rightmost) = cells_of(&pager, big)?;
    let want_right: Vec<(PageId, Vec<u8>)> = (9..15)
        .zip(10..16)
        .map(|(child, key)| Ok((id(child)?, wide_key(key))))
        .collect::<anyhow::Result<_>>()?;
    ensure!(cells == want_right, "big: {cells:?}");
    ensure!(rightmost == id(15)?);
    ensure!(pager.free_page_count() == 1, "only the merged leaf is free");
    let expected: Entries = (1..16).map(|i| (wide_key(i), V.to_vec())).collect();
    ensure!(check_tree(&pager, tree)? == expected);
    verify_tree(&pager, tree)?;
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

#[test]
fn a_long_separator_splits_a_parent_full_of_short_ones() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    // The root has 47 cells with 2 byte keys: 470 of 480 bytes. Leaf 20 is
    // full of 100 byte keys, and leaf 21 is about to be left with one key.
    let long = |t: char| format!("20{t}{}", ".".repeat(97)).into_bytes();
    let thirteen = [7u8; 13];
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut leaf_entries: Vec<Vec<(Vec<u8>, Vec<u8>)>> = Vec::new();
    for i in 0..48usize {
        let entries = match i {
            20 => ['a', 'b', 'c', 'd']
                .into_iter()
                .map(|t| (long(t), thirteen.to_vec()))
                .collect(),
            21 => vec![(b"21a".to_vec(), V.to_vec()), (b"21b".to_vec(), V.to_vec())],
            _ => vec![(format!("{i:02}a").into_bytes(), V.to_vec())],
        };
        model.extend(entries.iter().cloned());
        leaf_entries.push(entries);
    }
    let mut next = None;
    let mut ids = Vec::new();
    for entries in leaf_entries.iter().rev() {
        let cells: Vec<(&[u8], &[u8])> = entries
            .iter()
            .map(|(key, value)| (key.as_slice(), value.as_slice()))
            .collect();
        let id = write_leaf(&mut pager, &cells, next)?;
        ids.push(id);
        next = Some(id);
    }
    ids.reverse();
    let separators: Vec<Vec<u8>> = (1..48usize)
        .map(|i| format!("{i:02}").into_bytes())
        .collect();
    let root = internal_over(&mut pager, &ids, &separators)?;
    let tree = BTree::open(&pager, root)?;
    ensure!(height(&pager, root)? == 2);
    let pages_before = pager.page_count();

    // `[21b]` is underfull and its left sibling is full, so the two are cut
    // again and the separator `21` becomes a 100 byte key: the root has no
    // room for it and splits.
    ensure!(tree.delete(&mut pager, b"21a")?);
    model.remove(b"21a".as_slice());

    ensure!(tree.root() == root);
    ensure!(height(&pager, root)? == 3, "the root split");
    ensure!(pager.page_count() == pages_before.saturating_add(2));
    ensure!(pager.free_page_count() == 0);
    let expected: Entries = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    ensure!(check_tree(&pager, tree)? == expected);
    for (key, value) in &expected {
        ensure_value(get(tree, &pager, key)?.as_deref(), value, "key")?;
    }
    verify_tree(&pager, tree)?;
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

#[test]
fn a_separator_that_does_not_fit_in_the_parent_splits_it() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let long = |prefix: u8| -> Vec<u8> {
        let mut key = vec![b'.'; 100];
        if let Some(first) = key.first_mut() {
            *first = prefix;
        }
        key
    };
    let full = [9u8; 112];
    let filler = [4u8; 100];
    let (n1, n2, n3) = (long(b'n'), long(b'o'), long(b'p'));
    // `l2` holds `m` and three long keys: 120 + 3 * 110 bytes.
    let l2_entries: Vec<(&[u8], &[u8])> = vec![
        (b"m", &full),
        (n1.as_slice(), b"abc"),
        (n2.as_slice(), b"abc"),
        (n3.as_slice(), b"abc"),
    ];
    let tail_keys: Vec<Vec<u8>> = b"qrst".iter().copied().map(long).collect();
    // Leaves to the right of `l2`, one per long separator of the root.
    let tail = leaf_run(&mut pager, &tail_keys, None)?;
    let l2 = write_leaf(&mut pager, &l2_entries, tail.first().copied())?;
    let l1 = write_leaf(&mut pager, &[(b"a", &filler), (b"b", V)], Some(l2))?;

    let first_tail = tail_keys.first().context("tail key")?;
    let mut cells: Vec<(PageId, &[u8])> = vec![(l1, b"m"), (l2, first_tail.as_slice())];
    for (index, id) in tail.iter().enumerate().take(3) {
        let key = tail_keys.get(index + 1).context("key")?;
        cells.push((*id, key.as_slice()));
    }
    let last = *tail.last().context("tail")?;
    let root = write_internal(&mut pager, &cells, last)?;
    let tree = BTree::open(&pager, root)?;
    ensure!(height(&pager, root)? == 2);

    // 108 + 120 + 3 * 110 bytes do not fit in one page, so the pair is
    // redistributed and the new separator, a 100 byte key, replaces `m`. The
    // root has no room for it and splits: the tree grows by one level.
    ensure!(tree.delete(&mut pager, b"b")?);

    ensure!(tree.root() == root);
    ensure!(height(&pager, root)? == 3, "the root split");
    ensure!(pager.free_page_count() == 0);
    let mut expected: Entries = vec![
        (b"a".to_vec(), filler.to_vec()),
        (b"m".to_vec(), full.to_vec()),
    ];
    for key in [&n1, &n2, &n3] {
        expected.push((key.clone(), b"abc".to_vec()));
    }
    for key in &tail_keys {
        expected.push((key.clone(), V.to_vec()));
    }
    ensure!(check_tree(&pager, tree)? == expected);
    for (key, value) in &expected {
        ensure_value(get(tree, &pager, key)?.as_deref(), value, "key")?;
    }
    verify_tree(&pager, tree)?;
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

/// The leaf that holds `key`, found with the descent of a lookup.
fn leaf_for(pager: &Pager, tree: BTree, key: &[u8]) -> anyhow::Result<PageId> {
    let mut id = tree.root();
    loop {
        let buf = read(pager, id)?;
        match InternalNode::new(&buf) {
            Ok(node) => id = node.child_for(key)?.1,
            Err(_) => return Ok(id),
        }
    }
}

#[test]
fn a_redistribution_can_move_an_overflow_cell_between_leaves() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    // Cells of 104 bytes (key, 90 byte value, slot) and one overflow cell of
    // 22 bytes: the first five keys fill the left leaf (438 bytes). The sixth
    // key starts a right leaf (right-only append) and the seventh joins it.
    let inline = [3u8; 90];
    let big: Vec<u8> = (0..400u16)
        .map(|b| u8::try_from(b % 251).unwrap_or(0))
        .collect();
    let mut model = BTreeMap::new();
    for i in 0..7usize {
        let key = format!("key-{i:03}").into_bytes();
        let value = if i == 4 { big.clone() } else { inline.to_vec() };
        tree.put(&mut pager, &key, &value)?;
        model.insert(key, value);
    }
    let (overflow_pages, _) = ensure_no_leaks(&pager, tree)?;
    ensure!(overflow_pages > 0, "the test needs an overflow value");
    ensure!(leaf_chain(&pager, tree)?.len() == 2, "two leaves");
    let key = b"key-004";
    let home = leaf_for(&pager, tree, key)?;
    ensure!(home == leaf_for(&pager, tree, b"key-000")?, "left leaf");

    // The right leaf is left with one cell (104 bytes, underfull) and the pair
    // does not fit in one page: it is cut again by bytes and the overflow cell
    // moves to the right leaf, with its chain untouched.
    ensure!(tree.delete(&mut pager, b"key-006")?);
    model.remove(b"key-006".as_slice());

    ensure!(
        leaf_for(&pager, tree, key)? != home,
        "the cell did not move"
    );
    ensure!(
        pager.free_page_count() == 0,
        "a redistribution frees nothing"
    );
    ensure_value(get(tree, &pager, key)?.as_deref(), &big, "moved value")?;
    let (after, entries) = ensure_no_leaks(&pager, tree)?;
    ensure!(after == overflow_pages, "the chain changed");
    let expected: Entries = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    ensure!(entries == expected, "entries");
    verify_tree(&pager, tree)?;
    Ok(())
}

// ------------------------------------------------------------- bulk deletes

#[derive(Clone, Copy, Debug)]
enum Order {
    Ascending,
    Descending,
    Scrambled,
}

fn order_of(count: usize, order: Order) -> anyhow::Result<Vec<usize>> {
    (0..count)
        .map(|step| match order {
            Order::Ascending => Ok(step),
            Order::Descending => count
                .checked_sub(step.saturating_add(1))
                .context("underflow"),
            // 7919 is prime and `count` is not a multiple of it.
            Order::Scrambled => step
                .checked_mul(7919)
                .and_then(|n| n.checked_rem(count))
                .context("overflow"),
        })
        .collect()
}

/// Deletes every entry in `order`, checking the tree, that no node is empty
/// and the page accounting after every `every` deletes, and after each of the
/// last 40 (where the tree collapses).
fn delete_all_checking(
    pager: &mut Pager,
    tree: BTree,
    entries: &[(Vec<u8>, Vec<u8>)],
    order: Order,
    every: usize,
) -> anyhow::Result<()> {
    let mut model: BTreeMap<_, _> = entries.iter().cloned().collect();
    for (step, index) in order_of(entries.len(), order)?.into_iter().enumerate() {
        let (key, _) = entries.get(index).context("entry")?;
        ensure!(tree.delete(pager, key)?, "{order:?}: {key:?} was missing");
        model.remove(key);
        if step.is_multiple_of(every) || model.len() < 40 {
            let (_, found) = ensure_no_leaks(pager, tree)?;
            ensure!(found.len() == model.len(), "{order:?}: entry count");
            verify_tree(pager, tree)?;
        }
    }
    Ok(())
}

/// Checks that the tree collapsed to an empty root leaf with its old page id
/// and that every other page is free.
fn ensure_collapsed(pager: &Pager, tree: BTree, root: PageId, what: &str) -> anyhow::Result<()> {
    ensure!(tree.root() == root, "{what}: root id changed");
    let buf = read(pager, root)?;
    ensure!(LeafNode::new(&buf)?.cell_count() == 0, "{what}: root");
    ensure!(height(pager, root)? == 1, "{what}: height");
    // Page 0 is the file header and the root is the only node left.
    let others = pager.page_count().checked_sub(2).context("pages")?;
    ensure!(
        others == pager.free_page_count(),
        "{what}: {others} other pages, {} free",
        pager.free_page_count()
    );
    ensure!(check_tree(pager, tree)?.is_empty(), "{what}: entries left");
    Ok(())
}

/// A permutation of `0..count` (`count` is not a multiple of 7919).
fn scramble(step: usize, count: usize) -> anyhow::Result<usize> {
    step.checked_mul(7919)
        .and_then(|n| n.checked_rem(count))
        .context("overflow")
}

#[test]
fn deleting_every_key_of_a_two_level_tree_shrinks_it_to_an_empty_root() -> anyhow::Result<()> {
    for order in [Order::Ascending, Order::Descending, Order::Scrambled] {
        let (_dir, path) = temp_db()?;
        let mut pager = new_pager(&path)?;
        let tree = BTree::create(&mut pager)?;
        let entries = numbered_entries(120);
        for (key, value) in &entries {
            tree.put(&mut pager, key, value)?;
        }
        ensure!(height(&pager, tree.root())? == 2, "{order:?}: height");
        let root = tree.root();

        delete_all_checking(&mut pager, tree, &entries, order, 1)?;

        ensure_collapsed(&pager, tree, root, &format!("{order:?}"))?;
    }
    Ok(())
}

#[test]
fn deleting_every_key_of_a_three_level_tree_shrinks_it_to_an_empty_root() -> anyhow::Result<()> {
    for order in [Order::Ascending, Order::Descending, Order::Scrambled] {
        let (_dir, path) = temp_db()?;
        let mut pager = new_pager(&path)?;
        let tree = BTree::create(&mut pager)?;
        let count = 1000;
        let entries = numbered_entries(count);
        for step in 0..count {
            let (key, value) = entries.get(scramble(step, count)?).context("entry")?;
            tree.put(&mut pager, key, value)?;
        }
        ensure!(height(&pager, tree.root())? >= 3, "{order:?}: height");
        let root = tree.root();

        delete_all_checking(&mut pager, tree, &entries, order, 10)?;

        ensure_collapsed(&pager, tree, root, &format!("{order:?}"))?;
    }
    Ok(())
}

#[test]
fn a_bulk_loaded_tree_stays_valid_while_all_its_keys_are_deleted() -> anyhow::Result<()> {
    for order in [Order::Ascending, Order::Descending, Order::Scrambled] {
        let (_dir, path) = temp_db()?;
        let mut pager = new_pager(&path)?;
        let entries = numbered_entries(300);
        let root = bulk_load(&mut pager, &entries)?;
        let tree = BTree::open(&pager, root)?;
        ensure!(height(&pager, root)? >= 3, "{order:?}: height");

        delete_all_checking(&mut pager, tree, &entries, order, 1)?;

        ensure_collapsed(&pager, tree, root, &format!("{order:?}"))?;
        for (key, _) in &entries {
            ensure_missing(get(tree, &pager, key)?.as_deref(), "deleted key")?;
        }
    }
    Ok(())
}

// --------------------------------------------------------------- proptest

/// Number of distinct keys of the model test.
const KEYS: usize = 300;

/// Key `i`: a quarter of them are 100 bytes long, so internal nodes are small
/// and a 512 byte page tree gets three levels with a few hundred entries.
fn model_key(i: usize) -> Vec<u8> {
    let mut key = format!("k{i:04}").into_bytes();
    if i.is_multiple_of(4) {
        key.resize(100, b'.');
    }
    key
}

/// `len` bytes that depend on the position and on `seed`.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| {
            u8::try_from(i.wrapping_mul(31).wrapping_add(usize::from(seed)) % 251).unwrap_or(0)
        })
        .collect()
}

/// A full scan with the values read in full.
fn scan_all(tree: BTree, pager: &Pager, direction: Direction) -> anyhow::Result<Entries> {
    let mut cursor = tree.range(pager, Unbounded, Unbounded, direction)?;
    let mut out = Vec::new();
    while let Some(cell) = cursor.next_entry()? {
        let mut value = Vec::new();
        read_value(pager, cell.value, &mut value)?;
        out.push((cell.key.to_vec(), value));
    }
    Ok(out)
}

/// One operation: a selector (put 60% of the time, delete otherwise), a key
/// index, a value length and a seed.
type Op = (u8, usize, usize, u8);

/// Runs the operations on a tree and a `BTreeMap`, checking the tree after
/// each one. Returns the greatest height seen.
fn run_model(ops: &[Op]) -> anyhow::Result<usize> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let root = tree.root();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut max_height = 1;

    for &(selector, index, len, seed) in ops {
        let key = model_key(index.checked_rem(KEYS).context("no keys")?);
        if selector % 10 < 6 {
            let value = pattern(len, seed);
            tree.put(&mut pager, &key, &value)?;
            model.insert(key, value);
        } else {
            let deleted = tree.delete(&mut pager, &key)?;
            ensure!(
                deleted == model.remove(&key).is_some(),
                "delete of {key:?} returned {deleted}"
            );
        }
        let expected: Entries = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        ensure!(check_tree(&pager, tree)? == expected, "entries differ");
        verify_tree(&pager, tree)?;
        max_height = max_height.max(height(&pager, root)?);
    }

    let expected: Entries = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    ensure!(
        scan_all(tree, &pager, Direction::Forward)? == expected,
        "forward"
    );
    let mut reversed = expected;
    reversed.reverse();
    ensure!(
        scan_all(tree, &pager, Direction::Reverse)? == reversed,
        "reverse"
    );
    ensure_no_leaks(&pager, tree)?;

    for (step, key) in model.keys().enumerate() {
        ensure!(tree.delete(&mut pager, key)?, "{key:?} was missing");
        if step.is_multiple_of(4) {
            verify_tree(&pager, tree)?;
        }
    }
    ensure_collapsed(&pager, tree, root, "proptest")?;
    ensure_no_leaks(&pager, tree)?;
    Ok(max_height)
}

#[test]
fn the_model_test_reaches_three_levels() -> anyhow::Result<()> {
    // A fixed pseudo-random walk: the property test below does not promise
    // that every case is tall.
    let mut state = 12_345usize;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        state >> 33
    };
    let ops: Vec<Op> = (0..900)
        .map(|_| {
            let len = match next() % 3 {
                0 => next() % 30,
                1 => 60 + next() % 50,
                _ => 130 + next() % 570,
            };
            (
                u8::try_from(next() % 256).unwrap_or(0),
                next(),
                len,
                u8::try_from(next() % 256).unwrap_or(0),
            )
        })
        .collect();

    let max_height = run_model(&ops)?;

    ensure!(max_height >= 3, "the tallest tree had {max_height} levels");
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn prop_puts_and_deletes_keep_the_tree_valid_and_non_empty(
        ops in prop::collection::vec(
            (
                any::<u8>(),
                any::<usize>(),
                prop_oneof![0usize..30, 60usize..110, 130usize..700],
                any::<u8>(),
            ),
            1..450,
        ),
    ) {
        run_model(&ops).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
