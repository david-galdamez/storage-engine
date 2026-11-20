//! Whole-tree checker for the B+Tree integration tests.
//!
//! `check_tree` walks a tree using only the public node API and checks the
//! invariants of ADR 0005 that a split can break:
//!
//! - every node passes `validate()`;
//! - all leaves are at the same depth (invariant 2);
//! - the keys of every subtree are inside the bounds that its parent gives
//!   (invariant 4), passing the bounds down;
//! - the `next_leaf` chain visits the leaves in the same order as the walk,
//!   and the last leaf has no next leaf (invariant 6);
//! - the keys are strictly ascending across the whole tree (invariant 3);
//! - every overflow cell has a chain of the right structure and total length,
//!   and no page is used twice, by two chains or by a chain and a node
//!   (invariant 7). The entries returned hold the full values.
//!
//! `check_tree_pages` also returns the number of node pages and of overflow
//! pages of the tree, for the page accounting of the overflow tests.
//!
//! Each test binary that includes this module with `mod btree_check;` must use
//! every item, or `dead_code` fires.

use std::collections::HashSet;

use anyhow::{Context, ensure};
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::btree::internal::InternalNode;
use storage_engine_rs::storage::btree::leaf::LeafNode;
use storage_engine_rs::storage::btree::node::ValueRef;
use storage_engine_rs::storage::btree::overflow::{OverflowPage, overflow_capacity};
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::types::PageId;

/// A descent deeper than this is a cycle, not a tree.
const MAX_DEPTH: usize = 40;

/// `(key, value)` pairs.
pub type Entries = Vec<(Vec<u8>, Vec<u8>)>;

/// What the walk collects.
struct Walk {
    /// Depth of the first leaf found. All the others must match it.
    leaf_depth: Option<usize>,
    /// Each leaf in walk order (left to right) with its `next_leaf`.
    leaves: Vec<(PageId, Option<PageId>)>,
    entries: Entries,
    /// Every node page visited.
    node_pages: HashSet<PageId>,
    /// Every overflow page visited.
    overflow_pages: HashSet<PageId>,
}

/// Reads the chain of `len` bytes at `first`, checking its structure, and
/// returns the value. Records every page in `walk.overflow_pages`.
fn read_chain(pager: &Pager, len: u32, first: PageId, walk: &mut Walk) -> anyhow::Result<Vec<u8>> {
    let capacity = overflow_capacity(pager.page_size());
    let total = usize::try_from(len)?;
    ensure!(total > 0, "chain at {first}: overflow value of length 0");
    let mut value = Vec::new();
    let mut current = Some(first);
    while let Some(id) = current {
        ensure!(
            walk.overflow_pages.insert(id),
            "overflow page {id} is used twice"
        );
        ensure!(
            !walk.node_pages.contains(&id),
            "overflow page {id} is also a node"
        );
        let buf = read(pager, id)?;
        let page = OverflowPage::new(&buf).with_context(|| format!("overflow {id}"))?;
        page.validate().with_context(|| format!("overflow {id}"))?;
        let data = page.data()?;
        current = page.next()?;
        if current.is_some() {
            ensure!(
                data.len() == capacity,
                "overflow {id}: not full but not last"
            );
        }
        value.extend_from_slice(data);
        ensure!(
            value.len() <= total,
            "chain at {first} is longer than {total}"
        );
    }
    ensure!(
        value.len() == total,
        "chain at {first} has {} bytes, the cell says {total}",
        value.len()
    );
    Ok(value)
}

fn read(pager: &Pager, id: PageId) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; pager.page_size().as_usize()];
    pager.read_page(id, &mut buf)?;
    Ok(buf)
}

/// `lo <= key < hi`, where a missing bound is open.
fn within(key: &[u8], lo: Option<&[u8]>, hi: Option<&[u8]>) -> bool {
    lo.is_none_or(|lo| key >= lo) && hi.is_none_or(|hi| key < hi)
}

fn visit(
    pager: &Pager,
    id: PageId,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    depth: usize,
    walk: &mut Walk,
) -> anyhow::Result<()> {
    ensure!(depth < MAX_DEPTH, "page {id}: tree is too deep");
    let buf = read(pager, id)?;
    ensure!(walk.node_pages.insert(id), "node page {id} is used twice");
    ensure!(
        !walk.overflow_pages.contains(&id),
        "node {id} is also an overflow page"
    );

    if let Ok(node) = InternalNode::new(&buf) {
        node.validate().with_context(|| format!("internal {id}"))?;
        let count = node.cell_count();
        for pos in 0..=count {
            let child_lo = match pos.checked_sub(1) {
                Some(prev) => Some(node.key(prev)?),
                None => lo,
            };
            let child_hi = if pos == count {
                hi
            } else {
                let key = node.key(pos)?;
                ensure!(
                    within(key, lo, hi),
                    "internal {id}: separator {key:?} is outside its bounds"
                );
                Some(key)
            };
            let next_depth = depth.checked_add(1).context("depth overflow")?;
            visit(
                pager,
                node.child_at(pos)?,
                child_lo,
                child_hi,
                next_depth,
                walk,
            )?;
        }
        return Ok(());
    }

    let leaf = LeafNode::new(&buf).with_context(|| format!("page {id} is not a node"))?;
    leaf.validate().with_context(|| format!("leaf {id}"))?;
    match walk.leaf_depth {
        None => walk.leaf_depth = Some(depth),
        Some(expected) => ensure!(
            expected == depth,
            "leaf {id} is at depth {depth}, others at {expected}"
        ),
    }
    for index in 0..leaf.cell_count() {
        let cell = leaf.cell(index)?;
        ensure!(
            within(cell.key, lo, hi),
            "leaf {id}: key {:?} is outside its bounds",
            cell.key
        );
        match cell.value {
            ValueRef::Inline(value) => walk.entries.push((cell.key.to_vec(), value.to_vec())),
            ValueRef::Overflow { len, first_page } => {
                let value = read_chain(pager, len, first_page, walk)?;
                walk.entries.push((cell.key.to_vec(), value));
            }
        }
    }
    walk.leaves.push((id, leaf.next_leaf()?));
    Ok(())
}

/// Checks the whole tree (see the module doc) and returns all its entries in
/// key order.
pub fn check_tree(pager: &Pager, tree: BTree) -> anyhow::Result<Entries> {
    Ok(check_tree_pages(pager, tree)?.0)
}

/// Like `check_tree`, and also returns the number of node pages and the number
/// of overflow pages of the tree: `(entries, node_pages, overflow_pages)`.
pub fn check_tree_pages(pager: &Pager, tree: BTree) -> anyhow::Result<(Entries, usize, usize)> {
    let mut walk = Walk {
        leaf_depth: None,
        leaves: Vec::new(),
        entries: Vec::new(),
        node_pages: HashSet::new(),
        overflow_pages: HashSet::new(),
    };
    visit(pager, tree.root(), None, None, 0, &mut walk)?;

    // The chain: each leaf points to the next one of the walk, the last to
    // none.
    for (position, (id, next)) in walk.leaves.iter().enumerate() {
        let following = walk
            .leaves
            .get(position.checked_add(1).context("overflow")?)
            .map(|(next_id, _)| *next_id);
        ensure!(
            *next == following,
            "leaf {id}: next_leaf is {next:?}, the walk expects {following:?}"
        );
    }

    ensure!(
        walk.entries.windows(2).all(|w| w.first() < w.get(1)),
        "keys are not strictly ascending across the tree"
    );
    Ok((
        walk.entries,
        walk.node_pages.len(),
        walk.overflow_pages.len(),
    ))
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
