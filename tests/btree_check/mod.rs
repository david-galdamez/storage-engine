//! Whole-tree checker for the B+Tree integration tests.
//!
//! `check_tree` runs `verify_tree` (the library checker of the ADR 0005
//! invariants) and then a forward cursor scan that collects the entries with
//! their full values (overflow chains included).
//!
//! `check_tree_pages` also returns the number of node pages and of overflow
//! pages of the tree, for the page accounting of the overflow tests.
//!
//! Each test binary that includes this module with `mod btree_check;` must use
//! every item, or `dead_code` fires.

use std::ops::Bound;

use anyhow::ensure;
use storage_engine_rs::storage::btree::internal::InternalNode;
use storage_engine_rs::storage::btree::leaf::LeafNode;
use storage_engine_rs::storage::btree::overflow::read_value;
use storage_engine_rs::storage::btree::{BTree, Direction, verify_tree};
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::types::PageId;

/// A descent deeper than this is a cycle, not a tree.
const MAX_DEPTH: usize = 40;

/// `(key, value)` pairs.
pub type Entries = Vec<(Vec<u8>, Vec<u8>)>;

fn read(pager: &Pager, id: PageId) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; pager.page_size().as_usize()];
    pager.read_page(id, &mut buf)?;
    Ok(buf)
}

/// Checks the whole tree (see the module doc) and returns all its entries in
/// key order.
pub fn check_tree(pager: &Pager, tree: BTree) -> anyhow::Result<Entries> {
    Ok(check_tree_pages(pager, tree)?.0)
}

/// Like `check_tree`, and also returns the number of node pages and the number
/// of overflow pages of the tree: `(entries, node_pages, overflow_pages)`.
pub fn check_tree_pages(pager: &Pager, tree: BTree) -> anyhow::Result<(Entries, usize, usize)> {
    let stats = verify_tree(pager, tree)?;
    let mut entries = Entries::new();
    let mut cursor = tree.range(
        pager,
        Bound::Unbounded,
        Bound::Unbounded,
        Direction::Forward,
    )?;
    while let Some(cell) = cursor.next_entry()? {
        let mut value = Vec::new();
        read_value(pager, cell.value, &mut value)?;
        entries.push((cell.key.to_vec(), value));
    }
    ensure!(
        entries.len() == stats.entries,
        "the scan found {} entries, verify_tree counted {}",
        entries.len(),
        stats.entries
    );
    Ok((entries, stats.node_pages, stats.overflow_pages))
}

/// The leaves in order, found the way a forward scan would: down the leftmost
/// path, then along `next_leaf`.
pub fn leaf_chain(pager: &Pager, tree: BTree) -> anyhow::Result<Vec<PageId>> {
    let mut id = tree.root();
    for _ in 0..MAX_DEPTH {
        let buf = read(pager, id)?;
        match InternalNode::new(&buf) {
            Ok(node) => id = node.child_at(0)?,
            Err(_) => break,
        }
    }

    let max_leaves = usize::try_from(pager.page_count())?;
    let mut chain = Vec::new();
    let mut current = Some(id);
    while let Some(leaf_id) = current {
        ensure!(chain.len() < max_leaves, "next_leaf cycle");
        chain.push(leaf_id);
        let buf = read(pager, leaf_id)?;
        current = LeafNode::new(&buf)?.next_leaf()?;
    }
    Ok(chain)
}
