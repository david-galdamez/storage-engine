//! Whole-tree and whole-file structural checks.
//!
//! [`verify_tree`] walks one tree and checks the invariants of ADR 0005 that a
//! bug in a split, a merge or a delete can break:
//!
//! - every node passes `validate()` and has the right page type (the slotted
//!   page rules, keys strictly ascending, no zero child);
//! - every key is at most `max_key_len` long;
//! - every key and separator is inside the bounds that its ancestors give
//!   (invariant 4), which also gives the global order (invariant 3);
//! - all leaves are at the same depth (invariant 2), and the walk is never
//!   deeper than `MAX_TREE_HEIGHT`;
//! - the `next_leaf` chain visits the leaves in walk order and the last leaf
//!   has none (invariant 6);
//! - no internal node is without cells and no non-root leaf is empty
//!   (invariant 8);
//! - a value is inline if and only if it fits inline, and every overflow chain
//!   has valid pages, full pages except the last, and the length of the cell
//!   (invariant 7);
//! - no page is reached twice.
//!
//! It does not check that a node is not underfull: ADR 0005 guarantees no
//! minimum fill, so a tree built by sequential puts is valid with thin right
//! leaves.
//!
//! [`verify_file`] checks several trees at once, the free list, and that every
//! page of the file is in exactly one tree or on the free list.
//!
//! The walk is recursive, bounded by `MAX_TREE_HEIGHT`. Each frame owns one
//! page buffer, and the bounds of a child are borrowed from the buffers of its
//! ancestors, so no key is copied.

use std::collections::HashSet;

use crate::storage::btree::{
    error::{BTreeError, NodeError, Violation},
    internal::InternalNode,
    leaf::LeafNode,
    node::{ValueRef, fits_inline, max_key_len},
    overflow::{OverflowPage, overflow_capacity},
    tree::{BTree, MAX_TREE_HEIGHT, page_buffer},
};
use crate::storage::pager::{
    disk::Pager,
    page::{PageType, page_type},
    types::PageId,
};

/// Counts from one walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TreeStats {
    /// Levels from the root down to a leaf, counting both. 1 for a root leaf.
    pub height: usize,
    /// Number of cells in the leaves.
    pub entries: usize,
    /// Number of leaf and internal pages.
    pub node_pages: usize,
    /// Number of overflow pages.
    pub overflow_pages: usize,
}

/// State of a walk. `seen` is borrowed from `verify_file` and shared by all its trees.
struct Walk<'a> {
    // shortcut: HashSet of ids, switch to a bitmap if verify runs on large files
    seen: &'a mut HashSet<PageId>,
    /// Depth of the first leaf found. All the others must match it.
    leaf_depth: Option<usize>,
    /// The previous leaf of the walk and its `next_leaf`.
    prev_leaf: Option<(PageId, Option<PageId>)>,
    stats: TreeStats,
}

const fn invariant(page: PageId, violation: Violation) -> BTreeError {
    BTreeError::Invariant { page, violation }
}

const fn node_err(page: PageId) -> impl Fn(NodeError) -> BTreeError {
    move |source| BTreeError::Node { page, source }
}

/// `lo <= key < hi`, where a missing bound is open.
fn within(key: &[u8], lo: Option<&[u8]>, hi: Option<&[u8]>) -> bool {
    lo.is_none_or(|lo| key >= lo) && hi.is_none_or(|hi| key < hi)
}

/// Marks `id` as reached, or fails if it was already.
fn reach(state: &mut Walk<'_>, id: PageId) -> Result<(), BTreeError> {
    if state.seen.insert(id) {
        Ok(())
    } else {
        Err(invariant(id, Violation::PageReachedTwice))
    }
}

/// Counts one more page, saturating (a count can never reach `usize::MAX`).
const fn bump(n: &mut usize) {
    *n = n.saturating_add(1);
}

/// Checks that `key` is a valid key at cell `index` of the node `id`: not too
/// long and inside `[lo, hi)`.
fn check_key(
    pager: &Pager,
    id: PageId,
    index: usize,
    key: &[u8],
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
) -> Result<(), BTreeError> {
    let max = max_key_len(pager.page_size());
    if key.len() > max {
        return Err(invariant(
            id,
            Violation::KeyTooLarge {
                index,
                len: key.len(),
                max,
            },
        ));
    }
    if !within(key, lo, hi) {
        return Err(invariant(id, Violation::KeyOutOfBounds { index }));
    }
    Ok(())
}

fn walk(
    pager: &Pager,
    id: PageId,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    depth: usize,
    is_root: bool,
    state: &mut Walk<'_>,
) -> Result<(), BTreeError> {
    if depth >= MAX_TREE_HEIGHT {
        return Err(invariant(id, Violation::TooDeep));
    }
    reach(state, id)?;
    let mut buf = page_buffer(pager);
    pager.read_page(id, &mut buf)?;
    bump(&mut state.stats.node_pages);

    let is_internal = match page_type(&buf) {
        Ok(PageType::BTreeInternal) => true,
        Ok(PageType::BTreeLeaf) => false,
        Ok(found) => return Err(invariant(id, Violation::NotANode { found })),
        Err(e) => return Err(node_err(id)(e.into())),
    };
    if is_internal {
        let node = InternalNode::new(&buf).map_err(node_err(id))?;
        node.validate().map_err(node_err(id))?;
        let count = node.cell_count();
        if count == 0 {
            return Err(invariant(id, Violation::EmptyInternal));
        }
        let child_depth = depth.saturating_add(1);
        for pos in 0..=count {
            let child_lo = match pos.checked_sub(1) {
                Some(prev) => Some(node.key(prev).map_err(node_err(id))?),
                None => lo,
            };
            let child_hi = if pos == count {
                hi
            } else {
                let key = node.key(pos).map_err(node_err(id))?;
                check_key(pager, id, pos, key, lo, hi)?;
                Some(key)
            };
            let child = node.child_at(pos).map_err(node_err(id))?;
            walk(pager, child, child_lo, child_hi, child_depth, false, state)?;
        }
        return Ok(());
    }

    let leaf = LeafNode::new(&buf).map_err(node_err(id))?;
    leaf.validate().map_err(node_err(id))?;
    match state.leaf_depth {
        None => state.leaf_depth = Some(depth),
        Some(expected) if expected != depth => {
            return Err(invariant(id, Violation::LeafDepth { depth, expected }));
        }
        Some(_) => {}
    }
    let count = leaf.cell_count();
    if !is_root && count == 0 {
        return Err(invariant(id, Violation::EmptyLeaf));
    }
    for index in 0..count {
        let cell = leaf.cell(index).map_err(node_err(id))?;
        check_key(pager, id, index, cell.key, lo, hi)?;
        let page_size = pager.page_size();
        match cell.value {
            ValueRef::Inline(value) => {
                if !fits_inline(page_size, cell.key.len(), value.len()) {
                    return Err(invariant(id, Violation::NonCanonicalValue { index }));
                }
            }
            ValueRef::Overflow { len, first_page } => {
                let value_len = usize::try_from(len).unwrap_or(usize::MAX);
                if fits_inline(page_size, cell.key.len(), value_len) {
                    return Err(invariant(id, Violation::NonCanonicalValue { index }));
                }
                walk_chain(pager, id, index, len, first_page, state)?;
            }
        }
    }
    state.stats.entries = state.stats.entries.saturating_add(count);

    // Leaf chain: the previous leaf must point to this one.
    let next = leaf.next_leaf().map_err(node_err(id))?;
    if let Some((prev, found)) = state.prev_leaf
        && found != Some(id)
    {
        return Err(invariant(
            prev,
            Violation::LeafLink {
                found,
                expected: Some(id),
            },
        ));
    }
    state.prev_leaf = Some((id, next));
    Ok(())
}

/// Checks the chain of cell `index` of leaf `leaf`: `len` bytes starting at
/// `first`. Every page must be new, valid and full unless it is the last, and
/// the data must add up to `len`.
fn walk_chain(
    pager: &Pager,
    leaf: PageId,
    index: usize,
    len: u32,
    first: PageId,
    state: &mut Walk<'_>,
) -> Result<(), BTreeError> {
    let capacity = overflow_capacity(pager.page_size());
    let expected = usize::try_from(len).unwrap_or(usize::MAX);
    let mut found = 0usize;
    let mut buf = page_buffer(pager);
    let mut current = Some(first);
    // Terminates: a non-last page holds `capacity > 0` bytes and `found` is
    // capped by `expected`, so the chain has a bounded number of pages.
    while let Some(id) = current {
        reach(state, id)?;
        pager.read_page(id, &mut buf)?;
        bump(&mut state.stats.overflow_pages);
        let page = OverflowPage::new(&buf).map_err(node_err(id))?;
        page.validate().map_err(node_err(id))?;
        let data = page.data().map_err(node_err(id))?;
        current = page.next().map_err(node_err(id))?;
        if current.is_some() && data.len() != capacity {
            return Err(invariant(id, Violation::OverflowNotFull));
        }
        found = found.saturating_add(data.len());
        if found > expected {
            break;
        }
    }
    if found != expected || current.is_some() {
        return Err(invariant(
            leaf,
            Violation::OverflowLength {
                index,
                expected: len,
                found,
            },
        ));
    }
    Ok(())
}

/// Walks one tree with the shared `seen` set and returns its counts.
fn verify_one(
    pager: &Pager,
    tree: BTree,
    seen: &mut HashSet<PageId>,
) -> Result<TreeStats, BTreeError> {
    let mut state = Walk {
        seen,
        leaf_depth: None,
        prev_leaf: None,
        stats: TreeStats::default(),
    };
    walk(pager, tree.root(), None, None, 0, true, &mut state)?;
    if let Some((last, Some(found))) = state.prev_leaf {
        return Err(invariant(
            last,
            Violation::LeafLink {
                found: Some(found),
                expected: None,
            },
        ));
    }
    state.stats.height = state.leaf_depth.map_or(0, |d| d.saturating_add(1));
    Ok(state.stats)
}

/// Checks the structure of one tree (see the module doc) and returns its
/// counts.
///
/// # Errors
///
/// Returns [`BTreeError::Invariant`] with the page and the rule that failed,
/// [`BTreeError::Node`] if a page does not decode (including an unknown page
/// type byte), and [`BTreeError::Pager`] if a read fails.
pub fn verify_tree(pager: &Pager, tree: BTree) -> Result<TreeStats, BTreeError> {
    verify_one(pager, tree, &mut HashSet::new())
}

/// Checks every tree, the free list, and that each page of the file (except
/// the file header) is in exactly one tree or on the free list. `trees` must
/// list every tree of the file.
///
/// The counts are summed over the trees, except `height`, which is the
/// largest.
///
/// # Errors
///
/// The errors of [`verify_tree`] for any tree (a page shared by two trees is
/// [`Violation::PageReachedTwice`]), [`BTreeError::Pager`] if the free list is
/// corrupt, and [`Violation::PageAccounting`] if the pages of the trees plus
/// the free pages do not add up to the pages of the file.
pub fn verify_file(pager: &Pager, trees: &[BTree]) -> Result<TreeStats, BTreeError> {
    let mut seen = HashSet::new();
    let mut total = TreeStats::default();
    for &tree in trees {
        let stats = verify_one(pager, tree, &mut seen)?;
        total.height = total.height.max(stats.height);
        total.entries = total.entries.saturating_add(stats.entries);
        total.node_pages = total.node_pages.saturating_add(stats.node_pages);
        total.overflow_pages = total.overflow_pages.saturating_add(stats.overflow_pages);
    }
    pager.verify_freelist()?;
    let free = pager.free_page_count();
    let in_file = pager.page_count().saturating_sub(1);
    let accounted = u32::try_from(seen.len())
        .ok()
        .and_then(|n| n.checked_add(free));
    if accounted != Some(in_file) {
        return Err(invariant(
            PageId::HEADER,
            Violation::PageAccounting {
                reached: seen.len(),
                free,
                total: in_file,
            },
        ));
    }
    Ok(total)
}
