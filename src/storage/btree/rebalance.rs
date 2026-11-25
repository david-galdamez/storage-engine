//! Leaf rebalancing after a delete (ADR 0005, amendment 3).
//!
//! `delete` removes a cell in the leaf's buffer and then calls
//! [`rebalance_leaf`]. A leaf that is not the root and uses less than a quarter
//! of its usable bytes (see [`min_node_fill`]) is *underfull*. It is fixed with
//! a sibling that has the same parent (*Database Internals*, ch. 4,
//! Rebalancing):
//!
//! - **Merge**: if both leaves together fit in one page, the right one is
//!   merged into the left one. The left page keeps its id and takes the
//!   `next_leaf` of the right one, the right page is freed, and the parent
//!   loses the separator between them. Merging into the left leaf means no
//!   `prev` link and no neighbour of another parent is ever needed.
//! - **Redistribute**: otherwise the cells of both leaves are cut again by
//!   bytes (the same rule as a split) and the separator in the parent is
//!   replaced by the first key of the new right leaf.
//! - **Root collapse**: if a merge leaves the root (an internal node over
//!   leaves) with no cells, the merged leaf is written to the root page, which
//!   keeps its id, and both child pages are freed. The tree has one level.
//!
//! This step handles one level. Cases that would need work above the parent
//! leave the underfull leaf as it is, which is valid (ADR 0005, invariant 8):
//! a merge that would leave a non-root parent without cells, a
//! redistribution whose new separator does not fit in the parent, and any
//! underflow of the parent itself.
//!
//! All the pages are planned in memory and written through
//! [`Pending::write_all`]: changed pages bottom-up (the two leaves, then the
//! parent), then the emptied pages are freed. The caller frees the value's
//! overflow chain last. Moving a cell moves only its reference to an overflow
//! chain, never the chain (invariant 7).

use crate::storage::btree::{
    error::{BTreeError, NodeError},
    internal::{InternalNode, InternalNodeMut},
    leaf::{LeafNode, LeafNodeMut},
    node::{LeafCell, min_node_fill, page_size_of, usable_space},
    tree::{CellSequence, Crumb, Path, Pending, choose_split, page_buffer},
};
use crate::storage::pager::{disk::Pager, error::PageError, types::PageId};

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

/// Bytes used in the leaf `buffer`: its usable space minus its free space.
/// Cells and slots count, so the used bytes of two leaves add up exactly to
/// the used bytes of the leaf that holds all their cells.
fn used_bytes(buffer: &[u8]) -> Result<usize, NodeError> {
    let leaf = LeafNode::new(buffer)?;
    let usable = usable_space(page_size_of(buffer)?);

    Ok(usable.saturating_sub(leaf.slotted().total_free()))
}

/// Returns `true` if a non-root leaf with `used` bytes is underfull.
fn is_underfull(buffer: &[u8], used: usize) -> Result<bool, NodeError> {
    Ok(used < min_node_fill(page_size_of(buffer)?))
}

/// Plans the final content of the leaf `leaf_id`, whose cell was just removed
/// from `buffer`, and of every page that rebalancing changes, in `pending`.
///
/// A leaf that is the root (`path` is empty) or not underfull, or whose
/// rebalancing is one of the cases left for later (see the module docs), is
/// planned as it is. Nothing is written here.
///
/// # Errors
///
/// - [`BTreeError::Pager`] if reading the parent or the sibling fails.
/// - [`BTreeError::Node`] if a page does not decode, or a merge does not fit
///   its page, which the size check rules out in a valid tree.
/// - [`BTreeError::CorruptTree`] if the parent has no cells or does not lead
///   to the leaf, or a collapsing root has a leaf with a next leaf.
pub(super) fn rebalance_leaf(
    pager: &Pager,
    path: &Path,
    leaf_id: PageId,
    mut buffer: Vec<u8>,
    pending: &mut Pending,
) -> Result<(), BTreeError> {
    let node_error = |source| BTreeError::Node {
        page: leaf_id,
        source,
    };

    if let [parents @ .., parent] = path.as_slice() {
        let used = used_bytes(&buffer).map_err(node_error)?;
        if is_underfull(&buffer, used).map_err(node_error)? {
            match try_rebalance(pager, *parent, parents.is_empty(), leaf_id, buffer, pending)? {
                None => return Ok(()),
                Some(unchanged) => buffer = unchanged,
            }
        }
    }

    pending.changed.push((leaf_id, buffer));

    Ok(())
}

/// The pair of sibling leaves that rebalancing works on, and their parent.
struct Pair {
    /// The parent page id, for errors and for the pending write.
    parent_id: PageId,
    /// The parent page.
    parent: Vec<u8>,
    /// Index in the parent of the separator between the pair.
    sep: usize,
    left_id: PageId,
    right_id: PageId,
    /// `true` if the underfull leaf is the right one of the pair.
    leaf_is_right: bool,
}

/// Tries to merge or redistribute the underfull leaf `leaf_id` (`leaf_buf`)
/// with a sibling. Returns `None` if the pages were planned in `pending`, or
/// gives `leaf_buf` back, unchanged, if this case is not handled yet.
fn try_rebalance(
    pager: &Pager,
    crumb: Crumb,
    parent_is_root: bool,
    leaf_id: PageId,
    leaf_buf: Vec<u8>,
    pending: &mut Pending,
) -> Result<Option<Vec<u8>>, BTreeError> {
    let parent_error = |source| BTreeError::Node {
        page: crumb.page,
        source,
    };

    let mut parent = page_buffer(pager);
    pager.read_page(crumb.page, &mut parent)?;
    let view = InternalNode::new(&parent).map_err(parent_error)?;
    let count = view.cell_count();
    let reason = if count == 0 {
        Some("an internal node has no cells")
    } else if view.child_at(crumb.pos).map_err(parent_error)? != leaf_id {
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

    // The left sibling if there is one, otherwise the right one.
    let leaf_is_right = crumb.pos > 0;
    let sep = crumb.pos.saturating_sub(1);
    let sibling_pos = if leaf_is_right {
        sep
    } else {
        crumb.pos.saturating_add(1)
    };
    let sibling_id = view.child_at(sibling_pos).map_err(parent_error)?;
    let (left_id, right_id) = if leaf_is_right {
        (sibling_id, leaf_id)
    } else {
        (leaf_id, sibling_id)
    };

    let mut sibling = page_buffer(pager);
    pager.read_page(sibling_id, &mut sibling)?;

    let node_error = |page, source| BTreeError::Node { page, source };
    let leaf_used = used_bytes(&leaf_buf).map_err(|source| node_error(leaf_id, source))?;
    let sibling_used = used_bytes(&sibling).map_err(|source| node_error(sibling_id, source))?;
    let usable = usable_space(page_size_of(&leaf_buf).map_err(parent_error)?);

    let pair = Pair {
        parent_id: crumb.page,
        parent,
        sep,
        left_id,
        right_id,
        leaf_is_right,
    };
    if leaf_used.saturating_add(sibling_used) > usable {
        return redistribute_pair(pair, leaf_buf, &sibling, pending);
    }
    if count == 1 && !parent_is_root {
        // The parent would be left with no cells: step 11b.
        return Ok(Some(leaf_buf));
    }

    let (left, right) = if leaf_is_right {
        (sibling, leaf_buf)
    } else {
        (leaf_buf, sibling)
    };
    merge_pair(pair, left, &right, count == 1, pending)?;

    Ok(None)
}

/// Plans the merge of `right` into `left` (the pages of the pair). If
/// `collapse` is set, the parent is the root and has only this separator: the
/// merged leaf takes the root page and both leaf pages are freed. Otherwise
/// the left page is rewritten, the separator leaves the parent and the right
/// page is freed.
fn merge_pair(
    pair: Pair,
    mut left: Vec<u8>,
    right: &[u8],
    collapse: bool,
    pending: &mut Pending,
) -> Result<(), BTreeError> {
    let Pair {
        parent_id,
        mut parent,
        sep,
        left_id,
        right_id,
        ..
    } = pair;
    let left_error = |source| BTreeError::Node {
        page: left_id,
        source,
    };
    let parent_error = |source| BTreeError::Node {
        page: parent_id,
        source,
    };

    merge_into(&mut left, right).map_err(left_error)?;

    if collapse {
        // The root page id stays: the merged leaf is written to it. The two
        // leaves were the only ones, so there is no leaf after them.
        let next = LeafNode::new(&left)
            .and_then(|leaf| leaf.next_leaf())
            .map_err(left_error)?;
        if next.is_some() {
            return Err(BTreeError::CorruptTree {
                page: parent_id,
                reason: "the last two leaves of the tree have a next leaf",
            });
        }
        pending.changed.push((parent_id, left));
        pending.freed.push(left_id);
        pending.freed.push(right_id);

        return Ok(());
    }

    // After the cell is removed, the old right child sits at position `sep`
    // and must lead to the merged leaf.
    let mut node = InternalNodeMut::new(&mut parent).map_err(parent_error)?;
    node.remove(sep).map_err(parent_error)?;
    node.set_child_at(sep, left_id).map_err(parent_error)?;
    pending.changed.push((left_id, left));
    pending.changed.push((parent_id, parent));
    pending.freed.push(right_id);

    Ok(())
}

/// Plans the redistribution of the cells of the pair, whose underfull leaf is
/// `leaf_buf` and whose sibling is `sibling`. Returns `None` if it was planned,
/// or gives `leaf_buf` back if the new separator does not fit in the parent.
fn redistribute_pair(
    pair: Pair,
    leaf_buf: Vec<u8>,
    sibling: &[u8],
    pending: &mut Pending,
) -> Result<Option<Vec<u8>>, BTreeError> {
    let Pair {
        parent_id,
        mut parent,
        sep,
        left_id,
        right_id,
        leaf_is_right,
    } = pair;
    let parent_error = |source| BTreeError::Node {
        page: parent_id,
        source,
    };

    let (left, right) = if leaf_is_right {
        (sibling, leaf_buf.as_slice())
    } else {
        (leaf_buf.as_slice(), sibling)
    };
    let (new_left, new_right) = redistribute(left, right).map_err(|source| BTreeError::Node {
        page: left_id,
        source,
    })?;

    // The separator is the first key of the new right leaf, copied into one
    // owned vector (ADR 0005, amendment 1) because the leaf moves into the
    // pending writes.
    let mut separator = Vec::new();
    separator.extend_from_slice(
        LeafNode::new(&new_right)
            .and_then(|leaf| leaf.key(0))
            .map_err(|source| BTreeError::Node {
                page: right_id,
                source,
            })?,
    );

    // Replace separator `sep`: the child at `sep + 1` stays the right leaf.
    let mut node = InternalNodeMut::new(&mut parent).map_err(parent_error)?;
    node.remove(sep).map_err(parent_error)?;
    match node.insert(sep, &separator, left_id) {
        Ok(()) => {}
        // The new separator is longer than the old one and does not fit: step
        // 11b splits the parent. The parent buffer is dropped.
        Err(NodeError::Page(PageError::PageFull { .. })) => return Ok(Some(leaf_buf)),
        Err(source) => return Err(parent_error(source)),
    }
    pending.changed.push((left_id, new_left));
    pending.changed.push((right_id, new_right));
    pending.changed.push((parent_id, parent));

    Ok(None)
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
