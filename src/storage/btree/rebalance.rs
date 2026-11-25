//! Rebalancing after a delete (ADR 0005, amendment 3).
//!
//! `delete` removes a cell in the leaf's buffer and then calls
//! [`rebalance_leaf`]. A node that is not the root and uses less than a quarter
//! of its usable bytes (see [`min_node_fill`]) is *underfull*. It is fixed with
//! a sibling that has the same parent (*Database Internals*, ch. 4,
//! Rebalancing). The same two moves work for leaves and for internal nodes:
//!
//! - **Merge**: if both nodes together fit in one page, the right one is
//!   merged into the left one. The left page keeps its id, the right page is
//!   freed, and the parent loses the separator between them. For leaves the
//!   left page also takes the `next_leaf` of the right one. For internal nodes
//!   the separator is *pulled down*: the merged node holds the cells of the
//!   left node, then the separator with the rightmost child of the left node
//!   as its child, then the cells of the right node. Merging into the left
//!   node means no `prev` link and no neighbour of another parent is needed.
//! - **Redistribute**: otherwise the cells of both nodes are cut again by
//!   bytes (the same rule as a split) and the separator in the parent is
//!   replaced. For leaves the new separator is the first key of the new right
//!   leaf. For internal nodes it is a *rotation*: the cells of both nodes and
//!   the old separator are one sequence, and the middle key of the cut goes up
//!   and stays in neither node.
//!
//! A merge removes a cell from the parent, so the parent can become underfull
//! in turn. The walk then continues one level up with the next breadcrumb,
//! with the parent's buffer carried upward, until a node is not underfull, the
//! root is reached, or a redistribution ends it (the parent kept its cells).
//! A non-root internal node with no cells is allowed only while it is being
//! rebalanced.
//!
//! - **Root collapse**: if a merge leaves the root with no cells, the merged
//!   child (a leaf or an internal node) is written to the root page, which
//!   keeps its id, and both child pages are freed. The tree loses one level.
//! - **Parent split**: a redistribution can give the parent a longer separator
//!   than the one it replaces, and the parent may not have room for it. The
//!   parent is then split like in `put` and the split goes up through the
//!   breadcrumbs, which can allocate pages and, rarely, add a level. The walk
//!   ends there.
//!
//! All the pages are planned in memory and written through
//! [`Pending::write_all`]: new pages first, then the changed pages bottom-up
//! (the root last), then the emptied pages are freed. The caller frees the
//! value's overflow chain last. Moving a cell moves only its reference to an
//! overflow chain, never the chain (invariant 7).

use crate::storage::btree::{
    error::{BTreeError, NodeError},
    internal::{InternalNode, InternalNodeMut},
    leaf::{LeafNode, LeafNodeMut},
    node::{INTERNAL_CELL_HEADER_SIZE, LeafCell, min_node_fill, page_size_of, usable_space},
    tree::{
        CellSequence, Crumb, InternalSequence, InternalTarget, Path, Pending, build_halves,
        choose_internal_split, choose_split, fill_internal, page_buffer, split_parent,
    },
};
use crate::storage::pager::{
    disk::Pager,
    error::PageError,
    page::{PageType, page_type},
    slotted::{SLOT_SIZE, SlottedPage},
    types::PageId,
};

/// The cells of two sibling leaves, left then right, in key order, without
/// copying them: entry `j` is cell `j` of the left leaf, or cell `j - n` of
/// the right leaf, where `n` is the number of cells of the left leaf.
pub(super) struct LeafPair<'a> {
    left: LeafNode<'a>,
    right: LeafNode<'a>,
}

impl CellSequence for LeafPair<'_> {
    fn len(&self) -> usize {
        self.left
            .cell_count()
            .saturating_add(self.right.cell_count())
    }

    fn entry(&self, j: usize) -> Result<LeafCell<'_>, NodeError> {
        let left_count = self.left.cell_count();
        if j < left_count {
            self.left.cell(j)
        } else {
            self.right.cell(j.saturating_sub(left_count))
        }
    }
}

/// The cells of two sibling internal nodes and the separator between them, in
/// key order, without copying them. With `lc` cells in the left node, cell `j`
/// is cell `j` of the left node for `j < lc`, the separator for `j == lc` (its
/// child is the rightmost child of the left node), and cell `j - lc - 1` of
/// the right node after that. The child at position `len` is the rightmost
/// child of the right node.
struct InternalPair<'a> {
    left: InternalNode<'a>,
    right: InternalNode<'a>,
    separator: &'a [u8],
}

impl InternalSequence for InternalPair<'_> {
    fn len(&self) -> usize {
        self.left
            .cell_count()
            .saturating_add(1)
            .saturating_add(self.right.cell_count())
    }

    fn key(&self, j: usize) -> Result<&[u8], NodeError> {
        let left_count = self.left.cell_count();
        match j.cmp(&left_count) {
            std::cmp::Ordering::Less => self.left.key(j),
            std::cmp::Ordering::Equal => Ok(self.separator),
            std::cmp::Ordering::Greater => self
                .right
                .key(j.saturating_sub(left_count).saturating_sub(1)),
        }
    }

    fn child_at(&self, j: usize) -> Result<PageId, NodeError> {
        let left_count = self.left.cell_count();
        if j <= left_count {
            self.left.child_at(j)
        } else {
            self.right
                .child_at(j.saturating_sub(left_count).saturating_sub(1))
        }
    }
}

/// Bytes used in the node `buffer`: its usable space minus its free space.
/// Cells and slots count, so the used bytes of two leaves add up exactly to
/// the used bytes of the leaf that holds all their cells.
fn used_bytes(buffer: &[u8]) -> Result<usize, NodeError> {
    let page = SlottedPage::new(buffer)?;
    let usable = usable_space(page_size_of(buffer)?);

    Ok(usable.saturating_sub(page.total_free()))
}

/// Returns `true` if a non-root node with `used` bytes is underfull.
fn is_underfull(buffer: &[u8], used: usize) -> Result<bool, NodeError> {
    Ok(used < min_node_fill(page_size_of(buffer)?))
}

/// Plans the final content of the leaf `leaf_id`, whose cell was just removed
/// from `buffer`, and of every page that rebalancing changes, in `pending`.
///
/// A leaf that is the root (`path` is empty) or not underfull is planned as it
/// is. Otherwise the walk described in the module docs goes up the tree as far
/// as needed. Nothing is written here, but a parent split allocates pages.
///
/// # Errors
///
/// - [`BTreeError::Pager`] if reading a parent or a sibling fails, or an
///   allocation fails.
/// - [`BTreeError::Node`] if a page does not decode, or a merge does not fit
///   its page, which the size check rules out in a valid tree.
/// - [`BTreeError::CorruptTree`] if a parent has no cells or does not lead to
///   the child it was reached through, or a collapsing root has a leaf with a
///   next leaf.
pub(super) fn rebalance_leaf(
    pager: &mut Pager,
    path: &Path,
    leaf_id: PageId,
    buffer: Vec<u8>,
    pending: &mut Pending,
) -> Result<(), BTreeError> {
    let mut crumbs = path.as_slice();
    let mut node_id = leaf_id;
    let mut buffer = buffer;

    // `buffer` is the node that may be underfull: first the leaf, then the
    // parent that a merge shrank.
    loop {
        let node_error = |source| BTreeError::Node {
            page: node_id,
            source,
        };
        let Some((crumb, above)) = crumbs.split_last() else {
            // The root is never underfull.
            pending.changed.push((node_id, buffer));
            return Ok(());
        };
        let used = used_bytes(&buffer).map_err(node_error)?;
        if !is_underfull(&buffer, used).map_err(node_error)? {
            pending.changed.push((node_id, buffer));
            return Ok(());
        }

        let pair = load_pair(pager, *crumb, node_id, buffer)?;
        match fix_pair(pager, pair, above, pending)? {
            Some((parent_id, parent)) => {
                node_id = parent_id;
                buffer = parent;
                crumbs = above;
            }
            None => return Ok(()),
        }
    }
}

/// The parent of a pair, and where the separator between the pair sits.
struct Parent {
    id: PageId,
    buffer: Vec<u8>,
    /// Index in the parent of the separator between the pair.
    sep: usize,
}

/// Two sibling nodes that rebalancing works on, and their parent.
struct Pair {
    parent: Parent,
    left_id: PageId,
    right_id: PageId,
    left: Vec<u8>,
    right: Vec<u8>,
}

/// The new left page, the new right page and the separator that goes up to the
/// parent, after a rotation of internal nodes.
type Rotation = (Vec<u8>, Vec<u8>, Vec<u8>);

/// The new content of the two pages of a pair after a redistribution.
struct Halves {
    left_id: PageId,
    right_id: PageId,
    left: Vec<u8>,
    right: Vec<u8>,
}

/// Reads the parent and the sibling of the underfull node `node_id`
/// (`buffer`). The sibling is the left one if there is one, otherwise the
/// right one.
fn load_pair(
    pager: &Pager,
    crumb: Crumb,
    node_id: PageId,
    buffer: Vec<u8>,
) -> Result<Pair, BTreeError> {
    let parent_error = |source| BTreeError::Node {
        page: crumb.page,
        source,
    };

    let mut parent = page_buffer(pager);
    pager.read_page(crumb.page, &mut parent)?;
    let view = InternalNode::new(&parent).map_err(parent_error)?;
    let reason = if view.cell_count() == 0 {
        Some("an internal node has no cells")
    } else if view.child_at(crumb.pos).map_err(parent_error)? != node_id {
        Some("parent does not lead to the child it was reached through")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(BTreeError::CorruptTree {
            page: crumb.page,
            reason,
        });
    }

    let node_is_right = crumb.pos > 0;
    let sep = crumb.pos.saturating_sub(1);
    let sibling_pos = if node_is_right {
        sep
    } else {
        crumb.pos.saturating_add(1)
    };
    let sibling_id = view.child_at(sibling_pos).map_err(parent_error)?;
    let mut sibling = page_buffer(pager);
    pager.read_page(sibling_id, &mut sibling)?;

    let parent = Parent {
        id: crumb.page,
        buffer: parent,
        sep,
    };
    Ok(if node_is_right {
        Pair {
            parent,
            left_id: sibling_id,
            right_id: node_id,
            left: sibling,
            right: buffer,
        }
    } else {
        Pair {
            parent,
            left_id: node_id,
            right_id: sibling_id,
            left: buffer,
            right: sibling,
        }
    })
}

/// Merges or redistributes `pair`, planning the pages in `pending`. Returns
/// the parent (its id and buffer) if a merge shrank it and the walk must check
/// it, which the caller does, or `None` if everything is planned.
fn fix_pair(
    pager: &mut Pager,
    pair: Pair,
    above: &[Crumb],
    pending: &mut Pending,
) -> Result<Option<(PageId, Vec<u8>)>, BTreeError> {
    let Pair {
        parent,
        left_id,
        right_id,
        left,
        right,
    } = pair;
    let node_error = |page, source| BTreeError::Node { page, source };
    let is_leaf = page_type(&left) == Ok(PageType::BTreeLeaf);

    let usable = usable_space(page_size_of(&left).map_err(|e| node_error(left_id, e))?);
    let used = used_bytes(&left)
        .map_err(|e| node_error(left_id, e))?
        .saturating_add(used_bytes(&right).map_err(|e| node_error(right_id, e))?);
    // An internal merge also takes the separator down into the merged node.
    let pulled = if is_leaf {
        0
    } else {
        let key = InternalNode::new(&parent.buffer)
            .and_then(|node| node.key(parent.sep))
            .map_err(|e| node_error(parent.id, e))?;
        INTERNAL_CELL_HEADER_SIZE
            .saturating_add(key.len())
            .saturating_add(SLOT_SIZE)
    };

    if used.saturating_add(pulled) <= usable {
        let merged = if is_leaf {
            let mut merged = left;
            merge_into(&mut merged, &right).map_err(|e| node_error(left_id, e))?;
            merged
        } else {
            merge_internal(&parent.buffer, parent.sep, &left, &right)
                .map_err(|e| node_error(left_id, e))?
        };
        return merge_pair(parent, left_id, right_id, merged, above, pending);
    }

    let (new_left, new_right, separator) = if is_leaf {
        let (new_left, new_right) =
            redistribute(&left, &right).map_err(|e| node_error(left_id, e))?;
        // The separator is the first key of the new right leaf, copied into
        // one owned vector (ADR 0005, amendment 1) because the leaf moves into
        // the pending writes.
        let separator = LeafNode::new(&new_right)
            .and_then(|leaf| leaf.key(0).map(<[u8]>::to_vec))
            .map_err(|e| node_error(right_id, e))?;
        (new_left, new_right, separator)
    } else {
        rotate_internal(&parent.buffer, parent.sep, &left, &right)
            .map_err(|e| node_error(left_id, e))?
    };
    let halves = Halves {
        left_id,
        right_id,
        left: new_left,
        right: new_right,
    };
    replace_separator(pager, parent, halves, separator, above, pending)?;

    Ok(None)
}

/// Plans the merge of the pair into `merged`, which takes the left page. The
/// parent loses the separator, and the right page is freed.
///
/// If the parent is the root (`above` is empty) and has only this separator,
/// the tree collapses: `merged` takes the root page and both pair pages are
/// freed. Otherwise the parent is given back to the caller, which checks it
/// for underflow in turn.
fn merge_pair(
    parent: Parent,
    left_id: PageId,
    right_id: PageId,
    merged: Vec<u8>,
    above: &[Crumb],
    pending: &mut Pending,
) -> Result<Option<(PageId, Vec<u8>)>, BTreeError> {
    let Parent {
        id: parent_id,
        buffer: mut parent_buf,
        sep,
    } = parent;
    let parent_error = |source| BTreeError::Node {
        page: parent_id,
        source,
    };

    let count = InternalNode::new(&parent_buf)
        .map_err(parent_error)?
        .cell_count();
    if above.is_empty() && count == 1 {
        // The root page id stays: the merged node is written to it. If they
        // are leaves, they were the only ones, so there is no leaf after them.
        if page_type(&merged) == Ok(PageType::BTreeLeaf) {
            let next = LeafNode::new(&merged)
                .and_then(|leaf| leaf.next_leaf())
                .map_err(|source| BTreeError::Node {
                    page: left_id,
                    source,
                })?;
            if next.is_some() {
                return Err(BTreeError::CorruptTree {
                    page: parent_id,
                    reason: "the last two leaves of the tree have a next leaf",
                });
            }
        }
        pending.changed.push((parent_id, merged));
        pending.freed.push(left_id);
        pending.freed.push(right_id);

        return Ok(None);
    }

    // After the cell is removed, the old right child sits at position `sep`
    // and must lead to the merged node.
    let mut node = InternalNodeMut::new(&mut parent_buf).map_err(parent_error)?;
    node.remove(sep).map_err(parent_error)?;
    node.set_child_at(sep, left_id).map_err(parent_error)?;
    pending.changed.push((left_id, merged));
    pending.freed.push(right_id);

    Ok(Some((parent_id, parent_buf)))
}

/// Plans a redistribution: the pair pages get their new content and the
/// separator in the parent is replaced by `separator`. If the new separator
/// does not fit in the parent, the parent is split and the split goes up
/// through `above` (this can allocate pages and add a level).
fn replace_separator(
    pager: &mut Pager,
    parent: Parent,
    halves: Halves,
    mut separator: Vec<u8>,
    above: &[Crumb],
    pending: &mut Pending,
) -> Result<(), BTreeError> {
    let Parent {
        id: parent_id,
        buffer: mut parent_buf,
        sep,
    } = parent;
    let Halves {
        left_id,
        right_id,
        left,
        right,
    } = halves;
    let parent_error = |source| BTreeError::Node {
        page: parent_id,
        source,
    };

    // Replace separator `sep`: the child at `sep + 1` stays the right node.
    let inserted = {
        let mut node = InternalNodeMut::new(&mut parent_buf).map_err(parent_error)?;
        node.remove(sep).map_err(parent_error)?;
        node.insert(sep, &separator, left_id)
    };
    pending.changed.push((left_id, left));
    pending.changed.push((right_id, right));
    match inserted {
        Ok(()) => {
            pending.changed.push((parent_id, parent_buf));
            Ok(())
        }
        // The new separator is longer than the old one and does not fit. The
        // parent buffer has no cell `sep` now: the split puts it back.
        Err(NodeError::Page(PageError::PageFull { .. })) => {
            let target = InternalTarget {
                page: parent_id,
                buffer: &parent_buf,
                pos: sep,
                left_child: left_id,
                right_child: right_id,
                append: false,
            };
            split_parent(pager, above, pending, &target, &mut separator)
        }
        Err(source) => Err(parent_error(source)),
    }
}

/// Builds the node that merges the internal nodes `left` and `right` and the
/// separator `sep` of `parent` between them (see the module docs).
fn merge_internal(
    parent: &[u8],
    sep: usize,
    left: &[u8],
    right: &[u8],
) -> Result<Vec<u8>, NodeError> {
    let pair = InternalPair {
        left: InternalNode::new(left)?,
        right: InternalNode::new(right)?,
        separator: InternalNode::new(parent)?.key(sep)?,
    };
    let count = pair.len();
    let mut merged = vec![0u8; left.len()];
    fill_internal(&mut merged, &pair, 0..count, pair.child_at(count)?)?;

    Ok(merged)
}

/// Cuts the cells of the internal nodes `left` and `right` and the separator
/// between them again by bytes and returns the two new pages and the key that
/// goes up to the parent.
fn rotate_internal(
    parent: &[u8],
    sep: usize,
    left: &[u8],
    right: &[u8],
) -> Result<Rotation, NodeError> {
    let pair = InternalPair {
        left: InternalNode::new(left)?,
        right: InternalNode::new(right)?,
        separator: InternalNode::new(parent)?.key(sep)?,
    };
    if pair.len() < 3 {
        return Err(NodeError::CorruptNode {
            reason: "two sibling internal nodes to redistribute have fewer than two cells",
        });
    }
    let middle = choose_internal_split(&pair, false)?;
    let (new_left, new_right) = build_halves(&pair, middle, left.len())?;
    let promoted = pair.key(middle)?.to_vec();

    Ok((new_left, new_right, promoted))
}

/// Appends all the cells of the leaf `right` to the leaf `left` and gives the
/// merged leaf the `next_leaf` of `right`. The caller checked that they fit.
fn merge_into(left: &mut [u8], right: &[u8]) -> Result<(), NodeError> {
    let right = LeafNode::new(right)?;
    let next = right.next_leaf()?;

    let mut node = LeafNodeMut::new(left)?;
    let base = node.view().cell_count();
    for j in 0..right.cell_count() {
        let cell = right.cell(j)?;
        node.insert(base.saturating_add(j), cell.key, cell.value)?;
    }

    node.set_next_leaf(next)
}

/// Cuts the cells of the leaves `left` and `right` again by bytes (the rule of
/// a split, never an append) and returns the two new pages. The new left leaf
/// keeps the `next_leaf` of `left`, and the new right leaf the one of `right`.
fn redistribute(left: &[u8], right: &[u8]) -> Result<(Vec<u8>, Vec<u8>), NodeError> {
    let left_leaf = LeafNode::new(left)?;
    let right_leaf = LeafNode::new(right)?;
    let pair = LeafPair {
        left: left_leaf,
        right: right_leaf,
    };
    let total = pair.len();
    if total < 2 {
        return Err(NodeError::CorruptNode {
            reason: "two sibling leaves to redistribute have fewer than two cells",
        });
    }
    let cut = choose_split(&pair, false)?;

    let mut new_left = vec![0u8; left.len()];
    let mut new_right = vec![0u8; right.len()];
    LeafNodeMut::init(&mut new_left)?;
    LeafNodeMut::init(&mut new_right)?;
    let mut node = LeafNodeMut::new(&mut new_left)?;
    for j in 0..cut {
        let cell = pair.entry(j)?;
        node.insert(j, cell.key, cell.value)?;
    }
    node.set_next_leaf(left_leaf.next_leaf()?)?;
    let mut node = LeafNodeMut::new(&mut new_right)?;
    for j in cut..total {
        let cell = pair.entry(j)?;
        node.insert(j.saturating_sub(cut), cell.key, cell.value)?;
    }
    node.set_next_leaf(right_leaf.next_leaf()?)?;

    Ok((new_left, new_right))
}

#[cfg(test)]
mod tests {
    use anyhow::ensure;

    use super::*;
    use crate::storage::btree::node::ValueRef;
    use crate::storage::pager::types::PageSize;

    const PAGE: usize = 512;

    fn leaf_with(entries: &[(&[u8], &[u8])]) -> anyhow::Result<Vec<u8>> {
        let mut buffer = vec![0u8; PAGE];
        LeafNodeMut::init(&mut buffer)?;
        let mut leaf = LeafNodeMut::new(&mut buffer)?;
        for (index, (key, value)) in entries.iter().enumerate() {
            leaf.insert(index, key, ValueRef::Inline(value))?;
        }
        Ok(buffer)
    }

    #[test]
    fn leaf_pair_maps_indices_across_both_leaves() -> anyhow::Result<()> {
        let left = leaf_with(&[(b"a", b"1"), (b"b", b"2")])?;
        let right = leaf_with(&[(b"c", b"3"), (b"d", b"4"), (b"e", b"5")])?;
        let pair = LeafPair {
            left: LeafNode::new(&left)?,
            right: LeafNode::new(&right)?,
        };

        ensure!(pair.len() == 5);
        let keys = (0..5)
            .map(|j| pair.entry(j).map(|cell| cell.key))
            .collect::<Result<Vec<_>, _>>()?;
        ensure!(keys == [b"a".as_slice(), b"b", b"c", b"d", b"e"]);
        ensure!(pair.entry(5).is_err());

        Ok(())
    }

    #[test]
    fn leaf_pair_with_an_empty_side_reads_the_other() -> anyhow::Result<()> {
        let empty = leaf_with(&[])?;
        let full = leaf_with(&[(b"x", b"1"), (b"y", b"2")])?;
        let pair = LeafPair {
            left: LeafNode::new(&empty)?,
            right: LeafNode::new(&full)?,
        };

        ensure!(pair.len() == 2);
        ensure!(pair.entry(0)?.key == b"x");
        ensure!(pair.entry(1)?.key == b"y");

        Ok(())
    }

    fn internal_with(cells: &[(u32, &[u8])], rightmost: u32) -> anyhow::Result<Vec<u8>> {
        let mut buffer = vec![0u8; PAGE];
        InternalNodeMut::init(&mut buffer, PageId::new(rightmost))?;
        let mut node = InternalNodeMut::new(&mut buffer)?;
        for (index, (child, key)) in cells.iter().enumerate() {
            node.insert(index, key, PageId::new(*child))?;
        }
        Ok(buffer)
    }

    #[test]
    fn internal_pair_puts_the_separator_between_both_nodes() -> anyhow::Result<()> {
        let left = internal_with(&[(10, b"b"), (11, b"c")], 12)?;
        let right = internal_with(&[(20, b"x")], 21)?;
        let pair = InternalPair {
            left: InternalNode::new(&left)?,
            right: InternalNode::new(&right)?,
            separator: b"m",
        };

        ensure!(pair.len() == 4);
        let keys = (0..4).map(|j| pair.key(j)).collect::<Result<Vec<_>, _>>()?;
        ensure!(keys == [b"b".as_slice(), b"c", b"m", b"x"]);
        let children = (0..=4)
            .map(|j| pair.child_at(j).map(PageId::get))
            .collect::<Result<Vec<_>, _>>()?;
        // The child of the separator is the rightmost child of the left node.
        ensure!(children == [10, 11, 12, 20, 21]);
        ensure!(pair.key(4).is_err());

        Ok(())
    }

    #[test]
    fn merge_internal_pulls_the_separator_down() -> anyhow::Result<()> {
        let parent = internal_with(&[(1, b"m")], 2)?;
        let left = internal_with(&[(10, b"b")], 11)?;
        let right = internal_with(&[(20, b"x")], 21)?;

        let merged = merge_internal(&parent, 0, &left, &right)?;

        let node = InternalNode::new(&merged)?;
        ensure!(node.cell_count() == 3);
        ensure!(node.key(1)? == b"m");
        ensure!(node.child_at(1)? == PageId::new(11));
        ensure!(node.child_at(2)? == PageId::new(20));
        ensure!(node.rightmost_child()? == PageId::new(21));
        ensure!(used_bytes(&merged)? == (4 + 1 + 4) * 3);

        Ok(())
    }

    #[test]
    fn used_bytes_of_a_merged_leaf_is_the_sum_of_both() -> anyhow::Result<()> {
        let mut left = leaf_with(&[(b"a", b"11"), (b"b", b"222")])?;
        let right = leaf_with(&[(b"c", b"3"), (b"d", b"4444")])?;
        let expected = used_bytes(&left)?.saturating_add(used_bytes(&right)?);
        // Two cells of 3 + 1 + 2/3 bytes and two of 3 + 1 + 1/4: plus 4 each.
        ensure!(expected == (6 + 4) + (7 + 4) + (5 + 4) + (8 + 4));

        merge_into(&mut left, &right)?;

        ensure!(used_bytes(&left)? == expected);
        ensure!(LeafNode::new(&left)?.cell_count() == 4);

        Ok(())
    }

    #[test]
    fn merge_into_takes_the_next_leaf_of_the_right_leaf() -> anyhow::Result<()> {
        let mut left = leaf_with(&[(b"a", b"1")])?;
        LeafNodeMut::new(&mut left)?.set_next_leaf(Some(PageId::new(9)))?;
        let mut right = leaf_with(&[(b"b", b"2")])?;
        LeafNodeMut::new(&mut right)?.set_next_leaf(Some(PageId::new(12)))?;

        merge_into(&mut left, &right)?;

        ensure!(LeafNode::new(&left)?.next_leaf()? == Some(PageId::new(12)));

        Ok(())
    }

    #[test]
    fn underfull_is_a_quarter_of_the_usable_bytes() -> anyhow::Result<()> {
        // At 512 bytes, usable is 480 and the minimum is 120 used bytes. One
        // cell is 3 + 1 + value bytes, plus a 4-byte slot.
        let filler = [7u8; 112];
        let at_minimum = leaf_with(&[(b"k", &filler)])?;
        let below = leaf_with(&[(b"k", filler.get(..111).unwrap_or_default())])?;
        let empty = leaf_with(&[])?;

        ensure!(used_bytes(&at_minimum)? == 120);
        ensure!(!is_underfull(&at_minimum, 120)?);
        ensure!(used_bytes(&below)? == 119);
        ensure!(is_underfull(&below, 119)?);
        ensure!(used_bytes(&empty)? == 0);
        ensure!(is_underfull(&empty, 0)?);
        ensure!(min_node_fill(PageSize::new(512)?) == 120);

        Ok(())
    }

    #[test]
    fn merge_fits_when_the_sum_is_at_most_the_usable_bytes() -> anyhow::Result<()> {
        // Two leaves of 240 used bytes each fill the 480 usable bytes exactly.
        let value = [1u8; 112];
        let mut left = leaf_with(&[(b"a", &value), (b"b", &value)])?;
        let right = leaf_with(&[(b"c", &value), (b"d", &value)])?;
        let sum = used_bytes(&left)?.saturating_add(used_bytes(&right)?);
        ensure!(sum == 480);

        merge_into(&mut left, &right)?;

        ensure!(used_bytes(&left)? == 480);
        ensure!(LeafNode::new(&left)?.slotted().total_free() == 0);

        Ok(())
    }

    #[test]
    fn redistribute_cuts_by_bytes_and_keeps_the_links() -> anyhow::Result<()> {
        let value = [2u8; 100];
        let mut left = leaf_with(&[(b"a", &value)])?;
        LeafNodeMut::new(&mut left)?.set_next_leaf(Some(PageId::new(7)))?;
        let mut right = leaf_with(&[(b"b", &value), (b"c", &value), (b"d", &value)])?;
        LeafNodeMut::new(&mut right)?.set_next_leaf(Some(PageId::new(8)))?;

        let (new_left, new_right) = redistribute(&left, &right)?;

        let new_left = LeafNode::new(&new_left)?;
        let new_right = LeafNode::new(&new_right)?;
        ensure!(new_left.cell_count() == 2);
        ensure!(new_right.cell_count() == 2);
        ensure!(new_right.key(0)? == b"c");
        ensure!(new_left.next_leaf()? == Some(PageId::new(7)));
        ensure!(new_right.next_leaf()? == Some(PageId::new(8)));

        Ok(())
    }
}
