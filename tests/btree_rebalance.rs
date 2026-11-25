//! Tests for leaf rebalancing after `BTree::delete` (Phase 2, step 11a).
//! Page size is 512 bytes: 480 usable bytes, so a non-root leaf is underfull
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
//! - the cases left for step 11b (a merge that would empty a non-root parent,
//!   a separator that does not fit in the parent, any work above the parent):
//!   the tree stays valid and the pages do not change;
//!
//! and on trees built with `put` and `bulk_load`:
//!
//! - deleting every key of a two-level tree in ascending, descending and
//!   scrambled order ends with an empty root leaf with the same page id and
//!   every other page free;
//! - a three-level tree stays valid and without leaks while all its keys are
//!   deleted.

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

use anyhow::{Context, ensure};
use btree_check::{Entries, check_tree, leaf_chain};
use build::{bulk_load, write_internal, write_leaf};
use db::{new_pager, temp_db};
use leaks::ensure_no_leaks;
use lookup::{ensure_missing, ensure_value, get};
use shape::{blank, height, numbered_entries};
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::btree::internal::InternalNode;
use storage_engine_rs::storage::btree::leaf::LeafNode;
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

// ------------------------------------------------------ 11a restrictions

/// A height-3 tree: the root over `p1` and `p2`, `p1` over `[a b] [d e] [g h]`
/// (with `lone` set, over `[a b] [d e]` only), `p2` over `[m n] [s t]`.
struct Deep {
    tree: BTree,
    root: PageId,
    p1: PageId,
    p2: PageId,
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
        p1,
        p2,
        leaves,
    })
}

fn pages_of(pager: &Pager, ids: &[PageId]) -> anyhow::Result<Vec<Vec<u8>>> {
    ids.iter().map(|&id| read(pager, id)).collect()
}

#[test]
fn a_merge_that_would_empty_a_non_root_parent_is_left_for_11b() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let deep = deep(&mut pager, true)?;
    let others: Vec<PageId> = [deep.root, deep.p1, deep.p2]
        .into_iter()
        .chain(deep.leaves.iter().skip(1).copied())
        .collect();
    let before = pages_of(&pager, &others)?;

    // `[a b]` is underfull after the delete and would merge with `[d e]`, but
    // `p1` has one cell and is not the root.
    ensure!(deep.tree.delete(&mut pager, b"b")?);

    ensure!(pages_of(&pager, &others)? == before, "other pages changed");
    let first = *deep.leaves.first().context("leaf")?;
    ensure!(leaf_keys(&pager, first)? == keys(&[b"a"]));
    ensure!(pager.free_page_count() == 0);
    ensure!(height(&pager, deep.root)? == 3);
    ensure_no_leaks(&pager, deep.tree)?;
    Ok(())
}

#[test]
fn a_separator_that_does_not_fit_in_the_parent_is_left_for_11b() -> anyhow::Result<()> {
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
    let (n1, n2, n3) = (long(b'n'), long(b'o'), long(b'p'));
    // `l2` holds `m` and three long keys: 120 + 3 * 110 bytes.
    let l2_entries: Vec<(&[u8], &[u8])> = vec![
        (b"m", &full),
        (n1.as_slice(), b"abc"),
        (n2.as_slice(), b"abc"),
        (n3.as_slice(), b"abc"),
    ];
    let tail_keys: Vec<Vec<u8>> = [b'q', b'r', b's', b't'].into_iter().map(long).collect();
    // Leaves to the right of `l2`, one per long separator of the root.
    let mut next = None;
    let mut tail = Vec::new();
    for key in tail_keys.iter().rev() {
        let id = write_leaf(&mut pager, &[(key.as_slice(), V)], next)?;
        tail.push(id);
        next = Some(id);
    }
    tail.reverse();
    let l2 = write_leaf(&mut pager, &l2_entries, next)?;
    let filler = [4u8; 100];
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
    let mut all = vec![root, l2];
    all.extend(&tail);
    let before = pages_of(&pager, &all)?;

    // 108 + 120 + 3 * 110 bytes do not fit in one page, so the pair is
    // redistributed and the new separator, a 100 byte key, replaces `m`. The
    // root has no room for it.
    ensure!(tree.delete(&mut pager, b"b")?);

    ensure!(pages_of(&pager, &all)? == before, "pages changed");
    ensure!(leaf_keys(&pager, l1)? == keys(&[b"a"]));
    ensure!(pager.free_page_count() == 0);
    check_tree(&pager, tree)?;
    ensure_no_leaks(&pager, tree)?;
    Ok(())
}

#[test]
fn a_merge_does_not_rebalance_the_parent() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let deep = deep(&mut pager, false)?;
    let [l1, l2, l3, l4, l5]: [PageId; 5] = deep
        .leaves
        .clone()
        .try_into()
        .map_err(|_| anyhow::anyhow!("five leaves"))?;
    let untouched = [deep.root, deep.p2, l4, l5, l3];
    let before = pages_of(&pager, &untouched)?;

    ensure!(deep.tree.delete(&mut pager, b"e")?);

    // `[d]` merged into `[a b]`: `p1` is left with one cell, which is valid.
    ensure!(leaf_keys(&pager, l1)? == keys(&[b"a", b"b", b"d"]));
    let (cells, rightmost) = cells_of(&pager, deep.p1)?;
    ensure!(cells == [(l1, b"g".to_vec())], "p1: {cells:?}");
    ensure!(rightmost == l3);
    ensure!(pages_of(&pager, &untouched)? == before);
    ensure!(leaf_chain(&pager, deep.tree)? == [l1, l3, l4, l5]);
    ensure!(pager.free_page_count() == 1, "l2 {l2} is free");
    ensure!(height(&pager, deep.root)? == 3);
    ensure_no_leaks(&pager, deep.tree)?;
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

/// Deletes every entry in `order`, checking the tree and the page accounting
/// after each delete.
fn delete_all_checking(
    pager: &mut Pager,
    tree: BTree,
    entries: &[(Vec<u8>, Vec<u8>)],
    order: Order,
) -> anyhow::Result<()> {
    let mut model: BTreeMap<_, _> = entries.iter().cloned().collect();
    for index in order_of(entries.len(), order)? {
        let (key, _) = entries.get(index).context("entry")?;
        ensure!(tree.delete(pager, key)?, "{order:?}: {key:?} was missing");
        model.remove(key);
        let (_, found) = ensure_no_leaks(pager, tree)?;
        ensure!(found.len() == model.len(), "{order:?}: entry count");
    }
    Ok(())
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

        delete_all_checking(&mut pager, tree, &entries, order)?;

        ensure!(tree.root() == root);
        let buf = read(&pager, root)?;
        ensure!(LeafNode::new(&buf)?.cell_count() == 0, "{order:?}: root");
        ensure!(height(&pager, root)? == 1);
        // Page 0 is the file header and the root is the only node left.
        let others = pager.page_count().checked_sub(2).context("pages")?;
        ensure!(
            others == pager.free_page_count(),
            "{order:?}: {others} other pages, {} free",
            pager.free_page_count()
        );
        ensure!(check_tree(&pager, tree)?.is_empty());
    }
    Ok(())
}

#[test]
fn a_three_level_tree_stays_valid_while_all_its_keys_are_deleted() -> anyhow::Result<()> {
    for order in [Order::Ascending, Order::Descending, Order::Scrambled] {
        let (_dir, path) = temp_db()?;
        let mut pager = new_pager(&path)?;
        let entries = numbered_entries(300);
        let root = bulk_load(&mut pager, &entries)?;
        let tree = BTree::open(&pager, root)?;
        ensure!(height(&pager, root)? >= 3, "{order:?}: height");

        delete_all_checking(&mut pager, tree, &entries, order)?;

        ensure!(check_tree(&pager, tree)?.is_empty());
        for (key, _) in &entries {
            ensure_missing(get(tree, &pager, key)?.as_deref(), "deleted key")?;
        }
    }
    Ok(())
}
