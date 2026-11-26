//! The range cursor: ordered scans over a B+Tree.
//!
//! [`BTree::range`] seeks to one end of a key range and returns a [`Cursor`]
//! that walks the leaves in key order, forward or in reverse (*Database
//! Internals*, ch. 2 and 4: leaves are linked so range scans do not go back to
//! the parents).
//!
//! The cursor is a lending cursor (ADR 0005, option 7): [`Cursor::next_entry`]
//! returns a [`LeafCell`] borrowed from the cursor's own leaf buffer, valid
//! until the next call. It is therefore not an [`Iterator`]. The cursor holds
//! `&Pager` for its whole life, so no `put` can run while it is alive, and a
//! value in an overflow chain is read with [`read_value`] and the same pager.
//!
//! The position is a *gap*: the boundary between two cells of the current
//! leaf. Forward returns `cell[gap]` and then moves the gap right. Reverse
//! returns `cell[gap - 1]` and then moves the gap left.
//!
//! Forward steps follow the `next_leaf` link. There is no link to the left
//! (ADR 0005, invariant 6), so a reverse cursor keeps the breadcrumbs of its
//! descent and, at the left end of a leaf, climbs to the nearest ancestor that
//! has a child to the left and descends to the rightmost leaf of that child.
//!
//! [`read_value`]: crate::storage::btree::overflow::read_value

use std::ops::Bound;

use crate::storage::btree::{
    error::BTreeError,
    internal::InternalNode,
    leaf::LeafNode,
    node::{LeafCell, page_buffer},
    tree::{BTree, Crumb, MAX_TREE_HEIGHT, Path, Target},
};
use crate::storage::pager::{
    disk::Pager,
    page::{PageType, page_type},
    types::PageId,
};

pub use crate::storage::engine::Direction;

/// A cursor over a key range of one tree, created by [`BTree::range`].
///
/// [`Cursor::next_entry`] lends each cell until the next call, so the cursor
/// cannot be an `Iterator`. It borrows the pager, so the tree cannot change
/// while the cursor is alive:
///
/// ```compile_fail,E0502
/// # use std::ops::Bound;
/// # use storage_engine_rs::storage::btree::{BTree, Direction, error::BTreeError};
/// # use storage_engine_rs::storage::pager::disk::Pager;
/// fn demo(tree: BTree, pager: &mut Pager) -> Result<(), BTreeError> {
///     let mut cursor =
///         tree.range(pager, Bound::Unbounded, Bound::Unbounded, Direction::Forward)?;
///     tree.put(pager, b"key", b"value")?;
///     cursor.next_entry()?;
///     Ok(())
/// }
/// ```
///
/// The cursor is fused: after it returns `Ok(None)` or an error, every later
/// call returns `Ok(None)`.
#[derive(Debug)]
pub struct Cursor<'a> {
    pager: &'a Pager,
    direction: Direction,
    /// The bound at the far end of the scan: the upper bound going forward and
    /// the lower bound in reverse. The near bound is used only to seek.
    end: Bound<&'a [u8]>,
    /// The page of the current leaf.
    leaf: Vec<u8>,
    leaf_id: PageId,
    /// The position between cells in the current leaf (see the module docs).
    gap: usize,
    /// Breadcrumbs of the current leaf. Only a reverse cursor maintains them.
    path: Path,
    /// A copy of the page `parent_id`, so that going left across many leaves
    /// that share a parent reads the parent once. Empty until first used.
    parent: Vec<u8>,
    parent_id: Option<PageId>,
    /// Leaves read so far, to stop on a corrupt cycle.
    leaves: u32,
    done: bool,
}

impl BTree {
    /// Opens a cursor over the keys between `lower` and `upper`, in
    /// `direction` order. The bounds are borrowed and must outlive the cursor.
    /// An empty or inverted range is not an error: the cursor returns nothing.
    ///
    /// Reads the pages from the root down to the first leaf of the scan.
    ///
    /// # Errors
    ///
    /// - [`BTreeError::Pager`] if a read fails, a checksum is wrong or a child
    ///   is past the end of the file.
    /// - [`BTreeError::Node`] if a node on the path does not decode.
    /// - [`BTreeError::CorruptTree`] if the root or a child is not a B+Tree
    ///   node, or the descent is deeper than the maximum height.
    pub fn range<'a>(
        self,
        pager: &'a Pager,
        lower: Bound<&'a [u8]>,
        upper: Bound<&'a [u8]>,
        direction: Direction,
    ) -> Result<Cursor<'a>, BTreeError> {
        let (start, end) = match direction {
            Direction::Forward => (lower, upper),
            Direction::Reverse => (upper, lower),
        };
        let target = match (start, direction) {
            (Bound::Included(key) | Bound::Excluded(key), _) => Target::Key(key),
            (Bound::Unbounded, Direction::Forward) => Target::First,
            (Bound::Unbounded, Direction::Reverse) => Target::Last,
        };

        let mut leaf = page_buffer(pager);
        let mut path = Path::new(self.root());
        let leaf_id = self.descend(pager, target, &mut leaf, &mut path)?;

        let node_error = |source| BTreeError::Node {
            page: leaf_id,
            source,
        };
        let view = LeafNode::new(&leaf).map_err(node_error)?;
        let found = match start {
            Bound::Unbounded => Bound::Unbounded,
            Bound::Included(key) => Bound::Included(view.search(key).map_err(node_error)?),
            Bound::Excluded(key) => Bound::Excluded(view.search(key).map_err(node_error)?),
        };
        let gap = start_gap(direction, found, view.cell_count());

        Ok(Cursor {
            pager,
            direction,
            end,
            leaf,
            leaf_id,
            gap,
            path,
            parent: Vec::new(),
            parent_id: None,
            leaves: 1,
            done: false,
        })
    }
}

impl Cursor<'_> {
    /// Returns the next cell in the scan, or `None` at the end of the range.
    /// The cell borrows the cursor's leaf buffer and is valid until the next
    /// call. A value that is [`ValueRef::Overflow`](crate::storage::btree::node::ValueRef::Overflow)
    /// is read with [`read_value`](crate::storage::btree::overflow::read_value);
    /// a scan that does not need values never reads an overflow page.
    ///
    /// # Errors
    ///
    /// - [`BTreeError::Pager`] if a read fails or a checksum is wrong.
    /// - [`BTreeError::Node`] if a leaf, or an internal node on the way back
    ///   in a reverse scan, does not decode, or a `next_leaf` leads to a page
    ///   that is not a leaf.
    /// - [`BTreeError::CorruptTree`] if the leaf chain is longer than the
    ///   file, or a reverse climb meets a page that is not a B+Tree node or a
    ///   tree that is too deep.
    ///
    /// After an error the cursor is finished and returns `Ok(None)`.
    pub fn next_entry(&mut self) -> Result<Option<LeafCell<'_>>, BTreeError> {
        if self.done {
            return Ok(None);
        }

        let index = match self.advance() {
            Ok(Some(index)) => index,
            Ok(None) => {
                self.done = true;
                return Ok(None);
            }
            Err(error) => {
                self.done = true;
                return Err(error);
            }
        };

        // The position is final: only now the cell is borrowed from the buffer.
        match LeafNode::new(&self.leaf).and_then(|leaf| leaf.cell(index)) {
            Ok(cell) => Ok(Some(cell)),
            Err(source) => {
                self.done = true;
                Err(BTreeError::Node {
                    page: self.leaf_id,
                    source,
                })
            }
        }
    }

    /// Moves to the next cell in range and returns its index in the current
    /// leaf, or `None` if the range is finished.
    fn advance(&mut self) -> Result<Option<usize>, BTreeError> {
        loop {
            let node_error = |source| BTreeError::Node {
                page: self.leaf_id,
                source,
            };
            let leaf = LeafNode::new(&self.leaf).map_err(node_error)?;

            let (index, next_gap) = match self.direction {
                Direction::Forward => (Some(self.gap), self.gap.saturating_add(1)),
                Direction::Reverse => (self.gap.checked_sub(1), self.gap.saturating_sub(1)),
            };
            if let Some(index) = index.filter(|&index| index < leaf.cell_count()) {
                let key = leaf.key(index).map_err(node_error)?;
                if !within_end(self.direction, self.end, key) {
                    return Ok(None);
                }
                self.gap = next_gap;
                return Ok(Some(index));
            }

            // The leaf is used up in this direction.
            match self.direction {
                Direction::Forward => {
                    let Some(next) = leaf.next_leaf().map_err(node_error)? else {
                        return Ok(None);
                    };
                    self.count_leaf(next)?;
                    self.pager.read_page(next, &mut self.leaf)?;
                    self.leaf_id = next;
                    self.gap = 0;
                }
                Direction::Reverse => {
                    if !self.climb_left()? {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// Counts a leaf read and fails if more leaves were read than the file has
    /// pages, which only a cycle in the leaf links or the tree can cause.
    const fn count_leaf(&mut self, page: PageId) -> Result<(), BTreeError> {
        self.leaves = self.leaves.saturating_add(1);
        if self.leaves > self.pager.page_count() {
            return Err(BTreeError::CorruptTree {
                page,
                reason: "leaf chain is longer than the file",
            });
        }

        Ok(())
    }

    /// Reverse step across leaves: moves to the rightmost leaf of the subtree
    /// to the left of the current leaf. Returns `false` if the current leaf is
    /// the leftmost one.
    ///
    /// The crumbs from the bottom up are popped while they took child 0. The
    /// first crumb that did not take child 0 moves one child to the left, and
    /// the walk goes down from that child along the rightmost children,
    /// pushing a crumb for each level. Each pass of the loop either pops a
    /// crumb or moves a crumb left, so it ends (the depth is capped by
    /// [`Path::push`]).
    fn climb_left(&mut self) -> Result<bool, BTreeError> {
        let (page, pos) = loop {
            match self.path.last_mut() {
                None => return Ok(false),
                Some(crumb) if crumb.pos == 0 => {
                    self.path.pop();
                }
                Some(crumb) => {
                    crumb.pos = crumb.pos.saturating_sub(1);
                    crumb.last = false;
                    break (crumb.page, crumb.pos);
                }
            }
        };

        if self.parent_id != Some(page) {
            self.read_parent(page)?;
        }
        let node_error = |source| BTreeError::Node { page, source };
        let mut child = InternalNode::new(&self.parent)
            .and_then(|node| node.child_at(pos))
            .map_err(node_error)?;

        // Go down the rightmost children. Internal levels are read into
        // `parent` (the last one stays as the cache) and the leaf into `leaf`.
        for _ in 0..MAX_TREE_HEIGHT {
            self.pager.read_page(child, &mut self.leaf)?;
            match page_type(&self.leaf) {
                Ok(PageType::BTreeLeaf) => {
                    self.count_leaf(child)?;
                    self.leaf_id = child;
                    let node_error = |source| BTreeError::Node {
                        page: child,
                        source,
                    };
                    self.gap = LeafNode::new(&self.leaf).map_err(node_error)?.cell_count();

                    return Ok(true);
                }
                Ok(PageType::BTreeInternal) => {
                    std::mem::swap(&mut self.leaf, &mut self.parent);
                    self.parent_id = Some(child);
                    let node_error = |source| BTreeError::Node {
                        page: child,
                        source,
                    };
                    let node = InternalNode::new(&self.parent).map_err(node_error)?;
                    let last = node.cell_count();
                    let next = node.child_at(last).map_err(node_error)?;
                    self.path.push(Crumb {
                        page: child,
                        pos: last,
                        last: true,
                    })?;
                    child = next;
                }
                _ => {
                    return Err(BTreeError::CorruptTree {
                        page: child,
                        reason: "child is not a B+Tree node",
                    });
                }
            }
        }

        Err(BTreeError::CorruptTree {
            page: child,
            reason: "tree is deeper than the maximum height",
        })
    }

    /// Reads the internal node `page` into the parent cache.
    fn read_parent(&mut self, page: PageId) -> Result<(), BTreeError> {
        self.parent_id = None;
        if self.parent.len() != self.leaf.len() {
            self.parent.resize(self.leaf.len(), 0);
        }
        self.pager.read_page(page, &mut self.parent)?;
        self.parent_id = Some(page);

        Ok(())
    }
}

/// Returns the gap where a scan starts, given the cell count of the first
/// leaf and `start`, the near bound with the result of the leaf search for
/// its key (`Ok(i)` if found at `i`, `Err(i)` if it would be inserted at `i`).
///
/// Forward starts at the first cell in range: an included bound starts at the
/// key, and an excluded bound just after it. Reverse is the mirror: the first
/// cell returned is the one before the gap.
const fn start_gap(
    direction: Direction,
    start: Bound<Result<usize, usize>>,
    cell_count: usize,
) -> usize {
    match (direction, start) {
        (Direction::Forward, Bound::Unbounded) => 0,
        (Direction::Reverse, Bound::Unbounded) => cell_count,
        (Direction::Forward, Bound::Included(Ok(i) | Err(i)) | Bound::Excluded(Err(i)))
        | (Direction::Reverse, Bound::Excluded(Ok(i) | Err(i)) | Bound::Included(Err(i))) => i,
        (Direction::Forward, Bound::Excluded(Ok(i)))
        | (Direction::Reverse, Bound::Included(Ok(i))) => i.saturating_add(1),
    }
}

/// Returns `true` if `key` is inside the far bound of the scan: not past the
/// upper bound going forward, not before the lower bound in reverse.
fn within_end(direction: Direction, end: Bound<&[u8]>, key: &[u8]) -> bool {
    match (direction, end) {
        (_, Bound::Unbounded) => true,
        (Direction::Forward, Bound::Included(upper)) => key <= upper,
        (Direction::Forward, Bound::Excluded(upper)) => key < upper,
        (Direction::Reverse, Bound::Included(lower)) => key >= lower,
        (Direction::Reverse, Bound::Excluded(lower)) => key > lower,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use Bound::{Excluded, Included, Unbounded};
    use Direction::{Forward, Reverse};

    type Search = Bound<Result<usize, usize>>;

    #[test]
    fn start_gap_table() {
        // (direction, start, expected gap) with a leaf of 5 cells.
        let cases: [(Direction, Search, usize); 14] = [
            (Forward, Unbounded, 0),
            (Forward, Included(Ok(2)), 2),
            (Forward, Included(Err(2)), 2),
            (Forward, Excluded(Ok(2)), 3),
            (Forward, Excluded(Err(2)), 2),
            (Forward, Excluded(Ok(4)), 5),
            (Reverse, Unbounded, 5),
            (Reverse, Included(Ok(2)), 3),
            (Reverse, Included(Err(2)), 2),
            (Reverse, Excluded(Ok(2)), 2),
            (Reverse, Excluded(Err(2)), 2),
            (Reverse, Included(Ok(0)), 1),
            (Reverse, Excluded(Ok(0)), 0),
            (Reverse, Included(Err(0)), 0),
        ];
        for (direction, start, expected) in cases {
            assert_eq!(
                start_gap(direction, start, 5),
                expected,
                "{direction:?} {start:?}"
            );
        }
    }

    #[test]
    fn within_end_table() {
        let key: &[u8] = b"m";
        let cases: [(Direction, Bound<&[u8]>, bool); 14] = [
            (Forward, Unbounded, true),
            (Reverse, Unbounded, true),
            (Forward, Included(b"m"), true),
            (Forward, Included(b"n"), true),
            (Forward, Included(b"l"), false),
            (Forward, Excluded(b"m"), false),
            (Forward, Excluded(b"n"), true),
            (Forward, Excluded(b"l"), false),
            (Reverse, Included(b"m"), true),
            (Reverse, Included(b"l"), true),
            (Reverse, Included(b"n"), false),
            (Reverse, Excluded(b"m"), false),
            (Reverse, Excluded(b"l"), true),
            (Reverse, Excluded(b"n"), false),
        ];
        for (direction, end, expected) in cases {
            assert_eq!(
                within_end(direction, end, key),
                expected,
                "{direction:?} {end:?}"
            );
        }
    }
}
