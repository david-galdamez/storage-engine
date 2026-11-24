//! The B+Tree handle.
//!
//! A [`BTree`] is a small `Copy` value that stores only the page id of the
//! root of one tree. The root page id never changes while the tree lives
//! (ADR 0005, option 4: a root split moves the old root's contents to new
//! pages), so it is also the id of the tree and the catalog can store it.
//!
//! The handle does not own the pager. Every operation takes the pager as an
//! argument: `&Pager` for reads and `&mut Pager` for writes. Many trees share
//! one pager, and the borrow checker keeps writes and reads apart.
//!
//! This step has [`BTree::create`], [`BTree::open`], [`BTree::get`],
//! [`BTree::put`], [`BTree::delete`] and (in
//! [`cursor`](crate::storage::btree::cursor)) [`BTree::range`]. `put` splits a full leaf and propagates the split up through
//! the breadcrumbs of the descent: a full parent splits too, at any height,
//! and a split root keeps its page id. A value that does not fit inline goes
//! to a chain of overflow pages (see [`crate::storage::btree::overflow`]), and
//! `get` reads it back. `delete` removes a cell and frees its chain, without
//! rebalancing: leaves can become empty and stay linked (ADR 0005, invariant
//! 8). The descent is shared: `descend` finds the leaf for a key or the
//! first or last leaf of the tree, and the range cursor reuses it to seek.

use std::cmp::Ordering;
use std::ops::Range;

use crate::storage::btree::{
    error::{BTreeError, NodeError},
    internal::{InternalNode, InternalNodeMut},
    leaf::{LeafNode, LeafNodeMut},
    node::{
        INTERNAL_CELL_HEADER_SIZE, LeafCell, ValueRef, fits_inline, leaf_cell_len, max_key_len,
    },
    overflow::{free_chain, read_value, write_chain},
};
use crate::storage::pager::{
    disk::Pager,
    error::PageError,
    page::{PageType, page_type},
    slotted::SLOT_SIZE,
    types::PageId,
};

/// Maximum number of levels a descent visits, counting the root and the leaf.
///
/// Every internal node has at least one cell (ADR 0005, invariant 8), so it
/// has at least 2 children. A tree of height `h` therefore has at least
/// `2^(h-1)` leaves. Page ids are `u32`, so a file has fewer than `2^32`
/// pages and `h - 1 <= 32`, which gives `h <= 33`. A longer descent can only
/// be a corrupt file, for example a child pointing back up the tree.
pub(super) const MAX_TREE_HEIGHT: usize = 33;

/// One step of a descent: an internal node and the child position taken in it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Crumb {
    /// The internal node that was visited.
    pub(super) page: PageId,
    /// The child position taken in it, in `0..=N` (see
    /// [`InternalNode::child_for`]).
    pub(super) pos: usize,
    /// `true` if the descent took the rightmost child (`pos == N`).
    pub(super) last: bool,
}

/// The breadcrumbs of a descent: the internal nodes from the root down to the
/// parent of the leaf, with the child position taken in each (*Database
/// Internals*, ch. 4, Breadcrumbs).
///
/// It is filled by `BTree::descend` and has no heap allocation: a fixed array
/// of [`MAX_TREE_HEIGHT`] crumbs is enough, because a descent never visits
/// more levels. The path is empty when the root is the leaf. A leaf split reads
/// the crumbs from the bottom up to find the parent that receives the
/// separator, and the grandparent and above if the split propagates.
#[derive(Debug)]
pub(super) struct Path {
    crumbs: [Crumb; MAX_TREE_HEIGHT],
    len: usize,
}

impl Path {
    /// Creates an empty path. `filler` only fills the unused array slots.
    pub(super) const fn new(filler: PageId) -> Self {
        Self {
            crumbs: [Crumb {
                page: filler,
                pos: 0,
                last: false,
            }; MAX_TREE_HEIGHT],
            len: 0,
        }
    }

    /// Removes all the crumbs.
    pub(super) const fn clear(&mut self) {
        self.len = 0;
    }

    /// Appends a crumb at the bottom of the path.
    pub(super) fn push(&mut self, crumb: Crumb) -> Result<(), BTreeError> {
        let slot = self
            .crumbs
            .get_mut(self.len)
            .ok_or(BTreeError::CorruptTree {
                page: crumb.page,
                reason: "tree is deeper than the maximum height",
            })?;
        *slot = crumb;
        self.len = self.len.saturating_add(1);

        Ok(())
    }

    /// Removes and returns the bottom crumb, or `None` if the path is empty.
    pub(super) fn pop(&mut self) -> Option<Crumb> {
        self.len = self.len.checked_sub(1)?;

        self.crumbs.get(self.len).copied()
    }

    /// Returns the bottom crumb, or `None` if the path is empty.
    pub(super) fn last_mut(&mut self) -> Option<&mut Crumb> {
        let bottom = self.len.checked_sub(1)?;

        self.crumbs.get_mut(bottom)
    }

    /// Returns the crumbs from the root down to the parent of the leaf. The
    /// slice is empty when the leaf is the root.
    fn as_slice(&self) -> &[Crumb] {
        self.crumbs.get(..self.len).unwrap_or_default()
    }
}

/// Where a descent goes (see [`BTree::descend`]).
#[derive(Debug, Clone, Copy)]
pub(super) enum Target<'k> {
    /// The leaf whose subtree can hold this key (ADR 0005, invariant 4).
    Key(&'k [u8]),
    /// The leftmost leaf: child position 0 at every level.
    First,
    /// The rightmost leaf: child position N at every level.
    Last,
}

/// The cell that `put` wants to insert, and where it goes in the leaf.
#[derive(Debug, Clone, Copy)]
struct Insertion<'a> {
    /// Position of the new cell in the leaf, from `LeafNode::search`.
    index: usize,
    /// `true` if the key already existed. Its old cell is already removed
    /// from the leaf buffer.
    replaced: bool,
    key: &'a [u8],
    value: ValueRef<'a>,
}

/// The cells of a full leaf plus the new cell, in key order, without copying
/// them: entry `j` is read from the leaf or is the new cell, on demand.
struct Combined<'a> {
    leaf: LeafNode<'a>,
    insertion: Insertion<'a>,
}

impl<'a> Combined<'a> {
    /// Number of entries: the cells of the leaf plus the new one.
    fn len(&self) -> usize {
        self.leaf.cell_count().saturating_add(1)
    }

    /// Entry `j` of the combined sequence: `j < index` is leaf cell `j`,
    /// `j == index` is the new cell and `j > index` is leaf cell `j - 1`.
    fn entry(&self, j: usize) -> Result<LeafCell<'a>, NodeError> {
        match j.cmp(&self.insertion.index) {
            Ordering::Less => self.leaf.cell(j),
            Ordering::Equal => Ok(LeafCell {
                key: self.insertion.key,
                value: self.insertion.value,
            }),
            Ordering::Greater => self.leaf.cell(j.saturating_sub(1)),
        }
    }

    /// Bytes that entry `j` uses in a page: its cell plus its slot.
    fn entry_bytes(&self, j: usize) -> Result<usize, NodeError> {
        let cell = self.entry(j)?;

        Ok(leaf_cell_len(cell.key.len(), &cell.value).saturating_add(SLOT_SIZE))
    }
}

/// Handle of one B+Tree: the page id of its root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BTree {
    root: PageId,
}

impl BTree {
    /// Creates a new empty tree: allocates one page, formats it as an empty
    /// leaf with no next leaf (ADR 0005, invariant 1) and writes it. Does not
    /// sync.
    ///
    /// # Errors
    ///
    /// - [`BTreeError::Pager`] if the page cannot be allocated or written.
    /// - [`BTreeError::Node`] if formatting the leaf fails.
    pub fn create(pager: &mut Pager) -> Result<Self, BTreeError> {
        let root = pager.allocate_page()?;
        let mut buffer = page_buffer(pager);

        LeafNodeMut::init(&mut buffer).map_err(|source| BTreeError::Node { page: root, source })?;
        pager.write_page(root, &mut buffer)?;

        Ok(Self { root })
    }

    /// Opens the tree whose root is `root`. Reads the page and checks that it
    /// is a B+Tree leaf or internal node with a valid slotted header. Does not
    /// walk the tree.
    ///
    /// # Errors
    ///
    /// - [`BTreeError::InvalidRoot`] if `root` is page 0, is not below
    ///   `page_count`, or is a page that is not a B+Tree node.
    /// - [`BTreeError::Pager`] if the read fails or the checksum is wrong.
    /// - [`BTreeError::Node`] if the node has a corrupt slotted header.
    pub fn open(pager: &Pager, root: PageId) -> Result<Self, BTreeError> {
        if root.get() == 0 {
            return Err(BTreeError::InvalidRoot {
                page: root,
                reason: "page 0 is the file header",
            });
        }

        if root.get() >= pager.page_count() {
            return Err(BTreeError::InvalidRoot {
                page: root,
                reason: "page is past the end of the file",
            });
        }

        let mut buffer = page_buffer(pager);
        pager.read_page(root, &mut buffer)?;

        let node_error = |source| BTreeError::Node { page: root, source };
        match page_type(&buffer) {
            Ok(PageType::BTreeLeaf) => {
                LeafNode::new(&buffer).map_err(node_error)?;
            }
            Ok(PageType::BTreeInternal) => {
                InternalNode::new(&buffer).map_err(node_error)?;
            }
            _ => {
                return Err(BTreeError::InvalidRoot {
                    page: root,
                    reason: "page is not a B+Tree node",
                });
            }
        }

        Ok(Self { root })
    }

    /// Looks up `key`. If it is found, clears `out`, appends the value to it
    /// (reading the overflow chain if the value is out of line) and returns
    /// `true`. If it is not found, returns `false` and leaves `out` untouched.
    /// If reading an overflow chain fails, `out` is left empty.
    ///
    /// The descent follows ADR 0005, invariant 4, and reuses one page buffer
    /// for all levels.
    ///
    /// # Errors
    ///
    /// - [`BTreeError::Pager`] if a read fails, a checksum is wrong or a child
    ///   is past the end of the file.
    /// - [`BTreeError::Node`] if a node on the path does not decode.
    /// - [`BTreeError::CorruptTree`] if the root or a child is not a B+Tree
    ///   node, or the descent is deeper than the maximum height.
    /// - The errors of [`read_value`] if the value is in an overflow chain
    ///   that is corrupt: [`BTreeError::CorruptTree`] for a bad chain
    ///   structure, [`BTreeError::Node`] for an overflow page that does not
    ///   decode.
    pub fn get(self, pager: &Pager, key: &[u8], out: &mut Vec<u8>) -> Result<bool, BTreeError> {
        let mut buffer = page_buffer(pager);
        // `get` does not need the breadcrumbs.
        let mut path = Path::new(self.root);
        let leaf_id = self.descend(pager, Target::Key(key), &mut buffer, &mut path)?;

        let node_error = |source| BTreeError::Node {
            page: leaf_id,
            source,
        };
        let leaf = LeafNode::new(&buffer).map_err(node_error)?;
        let Ok(index) = leaf.search(key).map_err(node_error)? else {
            return Ok(false);
        };

        let value = leaf.cell(index).map_err(node_error)?.value;
        out.clear();
        read_value(pager, value, out)?;
        Ok(true)
    }

    /// Inserts `key` with `value`, or replaces the value if `key` exists
    /// (upsert). Returns `true` if a value was replaced and `false` if the key
    /// is new.
    ///
    /// The descent is the one of [`BTree::get`] and works for a tree of any
    /// height. If the leaf has room, the change is made in the leaf's page
    /// buffer and the page is written once, only after the change succeeded.
    ///
    /// If the leaf is full it is split (ADR 0005, invariant 5), and the split
    /// goes up through the breadcrumbs: a parent with no room for the new
    /// separator is split too, and so on up to the root. A root split keeps the
    /// root page id, and the tree grows one level. `put` never fails because a
    /// node is full.
    ///
    /// A value that does not fit inline (`fits_inline` is false) is written
    /// to a new overflow chain first, and the leaf cell then holds its length
    /// and first page. If the key already existed with a value in overflow,
    /// the old chain is freed only after the leaf (and any split pages) were
    /// written, so a crash can leak pages but never leaves a cell pointing to
    /// a freed page. Inline to overflow, overflow to inline and overflow to
    /// overflow all work. If the leaf insert or the split fails after the
    /// chain was written, the new chain's pages are orphaned (no WAL yet).
    ///
    /// A split writes several pages and there is no WAL yet (Phase 3). The
    /// whole split is planned in memory first, so an error while planning
    /// writes nothing, but it can leave allocated pages unused. A crash or an
    /// I/O error between the writes can leave the tree inconsistent. The pages
    /// are written in an order that keeps the tree readable as long as
    /// possible: new pages first, then the split leaf, then its ancestors
    /// bottom-up, so a modified root comes last. Does not sync.
    ///
    /// # Errors
    ///
    /// - [`BTreeError::KeyTooLarge`] if `key` is longer than `max_key_len`.
    /// - [`BTreeError::ValueTooLarge`] if `value` is longer than `u32::MAX`
    ///   bytes. Nothing is read or written.
    /// - [`BTreeError::Pager`] if a read, an allocation or a write fails or a
    ///   checksum is wrong. If this happens during a split, pages may already
    ///   be allocated or written.
    /// - [`BTreeError::Node`] if a node on the path does not decode.
    /// - [`BTreeError::CorruptTree`] if the root or a child is not a B+Tree
    ///   node, the descent is deeper than the maximum height, a node that
    ///   must split has too few entries, or a parent does not lead to the
    ///   child it was reached through, or the old overflow chain is corrupt
    ///   (checked while freeing it, after the new value was written, so the
    ///   put took effect and the pages before the bad one are freed).
    pub fn put(self, pager: &mut Pager, key: &[u8], value: &[u8]) -> Result<bool, BTreeError> {
        let page_size = pager.page_size();

        let max = max_key_len(page_size);
        if key.len() > max {
            return Err(BTreeError::KeyTooLarge {
                len: key.len(),
                max,
            });
        }
        let value_len = value_len_u32(value.len())?;

        let mut buffer = page_buffer(pager);
        let mut path = Path::new(self.root);
        let leaf_id = self.descend(pager, Target::Key(key), &mut buffer, &mut path)?;

        let node_error = |source| BTreeError::Node {
            page: leaf_id,
            source,
        };
        // The read view's borrow of `buffer` ends after its last use, so the
        // mutable view below can take the buffer afterwards.
        let leaf = LeafNode::new(&buffer).map_err(node_error)?;
        let (index, replaced, old_chain) = match leaf.search(key).map_err(node_error)? {
            Ok(index) => {
                let old_chain = overflow_chain(leaf.cell(index).map_err(node_error)?.value);
                (index, true, old_chain)
            }
            Err(index) => (index, false, None),
        };

        // The new chain is written before the leaf that points to it. The
        // old chain, if any, is freed only after the leaf was written.
        let value = if fits_inline(page_size, key.len(), value.len()) {
            ValueRef::Inline(value)
        } else {
            ValueRef::Overflow {
                len: value_len,
                first_page: write_chain(pager, value)?,
            }
        };

        let mut leaf = LeafNodeMut::new(&mut buffer).map_err(node_error)?;
        if replaced {
            leaf.remove(index).map_err(node_error)?;
        }
        match leaf.insert(index, key, value) {
            Ok(()) => {
                pager.write_page(leaf_id, &mut buffer)?;
                free_old_chain(pager, old_chain)?;
                return Ok(replaced);
            }
            // The leaf is full: split it below. For a replace, the old cell
            // is already out of the buffer, and the split starts from there.
            Err(NodeError::Page(PageError::PageFull { .. })) => {}
            Err(source) => return Err(node_error(source)),
        }

        let insertion = Insertion {
            index,
            replaced,
            key,
            value,
        };
        split_leaf(pager, &path, leaf_id, buffer, insertion)?;
        free_old_chain(pager, old_chain)?;

        Ok(replaced)
    }

    /// Removes `key`. Returns `true` if the key existed and was removed, and
    /// `false` if it was missing (nothing is written then). A key longer than
    /// `max_key_len` cannot be in the tree, so it returns `false` like
    /// [`BTree::get`]. The old value is not returned: call `get` first if it
    /// is needed.
    ///
    /// The descent is the one of [`BTree::get`]. The cell is removed in the
    /// leaf's page buffer and the page is written once. If the value was in an
    /// overflow chain, the chain is freed only after the leaf was written, so a
    /// crash can leak pages but never leaves a cell pointing to a freed page
    /// (ADR 0005, invariant 7).
    ///
    /// There is no rebalancing yet (Phase 2, step 11): the leaf is not
    /// compacted, an empty leaf stays in the tree with its `next_leaf`, and
    /// internal nodes are never touched (ADR 0005, invariant 8). Does not sync.
    ///
    /// Like `put`, `delete` needs `&mut Pager`, so it cannot run while a
    /// [`Cursor`](crate::storage::btree::cursor::Cursor) is alive:
    ///
    /// ```compile_fail,E0502
    /// # use std::ops::Bound;
    /// # use storage_engine_rs::storage::btree::{BTree, Direction, error::BTreeError};
    /// # use storage_engine_rs::storage::pager::disk::Pager;
    /// fn demo(tree: BTree, pager: &mut Pager) -> Result<(), BTreeError> {
    ///     let mut cursor =
    ///         tree.range(pager, Bound::Unbounded, Bound::Unbounded, Direction::Forward)?;
    ///     tree.delete(pager, b"key")?;
    ///     cursor.next_entry()?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`BTreeError::Pager`] if a read or a write fails or a checksum is
    ///   wrong.
    /// - [`BTreeError::Node`] if a node on the path does not decode.
    /// - [`BTreeError::CorruptTree`] if the root or a child is not a B+Tree
    ///   node, the descent is deeper than the maximum height, or the old
    ///   overflow chain is corrupt (checked while freeing it, after the leaf
    ///   was written, so the key is already deleted and the pages before the
    ///   bad one are freed).
    pub fn delete(self, pager: &mut Pager, key: &[u8]) -> Result<bool, BTreeError> {
        if key.len() > max_key_len(pager.page_size()) {
            return Ok(false);
        }

        let mut buffer = page_buffer(pager);
        // The path is not used until rebalancing (step 11).
        let mut path = Path::new(self.root);
        let leaf_id = self.descend(pager, Target::Key(key), &mut buffer, &mut path)?;

        let node_error = |source| BTreeError::Node {
            page: leaf_id,
            source,
        };
        let leaf = LeafNode::new(&buffer).map_err(node_error)?;
        let Ok(index) = leaf.search(key).map_err(node_error)? else {
            return Ok(false);
        };
        let old_chain = overflow_chain(leaf.cell(index).map_err(node_error)?.value);

        LeafNodeMut::new(&mut buffer)
            .map_err(node_error)?
            .remove(index)
            .map_err(node_error)?;
        pager.write_page(leaf_id, &mut buffer)?;
        free_old_chain(pager, old_chain)?;

        Ok(true)
    }

    /// Descends from the root to the leaf chosen by `target`: the leaf whose
    /// subtree can hold a key (ADR 0005, invariant 4), the first leaf or the
    /// last leaf. Returns the id of the leaf, whose page is left in `buffer`.
    /// All levels read into the same `buffer`, which must be one page long.
    ///
    /// `path` is cleared and then receives one crumb for each internal node on
    /// the way down, with the child position taken. It stays empty when the
    /// root is the leaf.
    pub(super) fn descend(
        self,
        pager: &Pager,
        target: Target<'_>,
        buffer: &mut [u8],
        path: &mut Path,
    ) -> Result<PageId, BTreeError> {
        let mut current = self.root;
        path.clear();

        for level in 0..MAX_TREE_HEIGHT {
            pager.read_page(current, buffer)?;

            match page_type(buffer) {
                Ok(PageType::BTreeLeaf) => return Ok(current),
                Ok(PageType::BTreeInternal) => {
                    let node_error = |source| BTreeError::Node {
                        page: current,
                        source,
                    };
                    let node = InternalNode::new(buffer).map_err(node_error)?;
                    let (pos, child) = match target {
                        Target::Key(key) => node.child_for(key).map_err(node_error)?,
                        Target::First => (0, node.child_at(0).map_err(node_error)?),
                        Target::Last => {
                            let last = node.cell_count();
                            (last, node.child_at(last).map_err(node_error)?)
                        }
                    };
                    path.push(Crumb {
                        page: current,
                        pos,
                        last: pos == node.cell_count(),
                    })?;
                    current = child;
                }
                _ => {
                    return Err(BTreeError::CorruptTree {
                        page: current,
                        reason: if level == 0 {
                            "root is not a B+Tree node"
                        } else {
                            "child is not a B+Tree node"
                        },
                    });
                }
            }
        }

        Err(BTreeError::CorruptTree {
            page: current,
            reason: "tree is deeper than the maximum height",
        })
    }

    /// Returns the root page id, which is also the id of the tree. It is
    /// stable for the life of the tree.
    #[must_use]
    pub const fn root(self) -> PageId {
        self.root
    }
}

/// Converts a value length to the `u32` a leaf cell stores.
///
/// # Errors
///
/// Returns [`BTreeError::ValueTooLarge`] if `len` is more than `u32::MAX`.
fn value_len_u32(len: usize) -> Result<u32, BTreeError> {
    u32::try_from(len).map_err(|_| BTreeError::ValueTooLarge {
        len,
        max: usize::try_from(u32::MAX).unwrap_or(usize::MAX),
    })
}

/// The length and first page of the chain of `value`, or `None` if the value
/// is inline.
const fn overflow_chain(value: ValueRef<'_>) -> Option<(u32, PageId)> {
    match value {
        ValueRef::Overflow { len, first_page } => Some((len, first_page)),
        ValueRef::Inline(_) => None,
    }
}

/// Frees the chain of the value that `put` replaced or `delete` removed, if it
/// was in overflow. Called only after the new leaf (and any split pages) were
/// written.
fn free_old_chain(pager: &mut Pager, old_chain: Option<(u32, PageId)>) -> Result<(), BTreeError> {
    match old_chain {
        Some((len, first_page)) => free_chain(pager, len, first_page),
        None => Ok(()),
    }
}

/// Allocates a zeroed buffer of one page, owned by the caller (ADR 0005,
/// option 7, until the buffer pool exists).
pub(crate) fn page_buffer(pager: &Pager) -> Vec<u8> {
    vec![0u8; pager.page_size().as_usize()]
}

/// Pages planned by a split. Nothing is written until the whole split is
/// planned, so a failure while planning leaves the existing pages unchanged
/// (only allocated pages can be left unused).
#[derive(Debug, Default)]
struct Pending {
    /// Pages allocated by the split, with their final content.
    new_pages: Vec<(PageId, Vec<u8>)>,
    /// Existing pages that change, in the order the split reaches them: the
    /// leaf first, then its ancestors, so a modified root comes last.
    changed: Vec<(PageId, Vec<u8>)>,
}

impl Pending {
    /// Writes the new pages first, then the existing pages bottom-up. If a
    /// write fails, the tree can be inconsistent until the WAL exists
    /// (Phase 3). The order keeps it readable as long as possible: until the
    /// first existing page is written, the old tree is intact.
    fn write_all(self, pager: &mut Pager) -> Result<(), BTreeError> {
        for (id, mut buffer) in self.new_pages.into_iter().chain(self.changed) {
            pager.write_page(id, &mut buffer)?;
        }

        Ok(())
    }
}

/// A node that was just split, in memory: `page` is its page id, `left` the
/// left half and `right` the right half. Neither half has a page id yet,
/// except that a non-root `left` keeps `page`.
struct SplitNode {
    page: PageId,
    left: Vec<u8>,
    right: Vec<u8>,
}

/// Splits the full leaf `leaf_id`, whose page is `buffer`, to make room for
/// `insertion`, and propagates the split up the tree (ADR 0005, invariants 4
/// to 6).
///
/// Let `L` be the leaf and `combined` the cells of `L` with the new cell
/// inserted at `insertion.index`. The algorithm:
///
/// 1. Choose `k`: `combined[0..k]` stays in `L` and `combined[k..]` goes to a
///    new right leaf `R`. Both parts are never empty.
///    - Right-only append: if `L` is the last leaf of the tree and the new key
///      goes after its last cell, `R` gets only the new cell and `L` keeps all
///      its cells (so sequential inserts fill leaves completely).
///    - Otherwise `k` is the smallest value for which the left part uses at
///      least half of the bytes of all the entries (cell plus slot).
/// 2. Build `R` in a new buffer. Its `next_leaf` is the old `next_leaf` of `L`.
/// 3. Shrink `L` in its buffer: remove the cells that moved, from the end
///    backwards, and insert the new cell if it belongs to the left part.
/// 4. The separator is the first key of `R`. `propagate_split` puts it in the
///    parent, splitting the parent (and its parent...) if it is full.
///
/// All the pages are planned in memory and written at the end, new pages
/// first and existing pages bottom-up (see [`Pending::write_all`]). No WAL
/// exists yet, so a failure after an allocation can leave unused pages in the
/// file, and a crash between the writes can leave the tree inconsistent until
/// Phase 3.
///
/// # Errors
///
/// - [`BTreeError::Pager`] if a read, an allocation or a write fails.
/// - [`BTreeError::Node`] if a node does not decode or a part does not fit in
///   its page, which the size limits (ADR 0005, option 6) rule out in a valid
///   tree.
/// - [`BTreeError::CorruptTree`] if a node that must split has too few
///   entries, or a parent does not point to the child it was reached through.
fn split_leaf(
    pager: &mut Pager,
    path: &Path,
    leaf_id: PageId,
    mut buffer: Vec<u8>,
    insertion: Insertion<'_>,
) -> Result<(), BTreeError> {
    let node_error = |source| BTreeError::Node {
        page: leaf_id,
        source,
    };

    // Steps 1 and 2: choose the split and build the right leaf. `combined`
    // borrows `buffer`, so this block ends before L is changed.
    let mut right = page_buffer(pager);
    let split_at = {
        let leaf = LeafNode::new(&buffer).map_err(node_error)?;
        let old_next = leaf.next_leaf().map_err(node_error)?;
        let combined = Combined { leaf, insertion };
        if combined.len() < 2 {
            return Err(BTreeError::CorruptTree {
                page: leaf_id,
                reason: "a leaf that must split has fewer than two entries",
            });
        }

        let append =
            old_next.is_none() && !insertion.replaced && insertion.index == leaf.cell_count();
        let split_at = choose_split(&combined, append).map_err(node_error)?;

        // The cells are decoded from L and encoded again in R. That costs a
        // little, but it reuses `LeafNodeMut::insert` and its limit checks,
        // with no new raw-cell API in the node layer.
        LeafNodeMut::init(&mut right).map_err(node_error)?;
        let mut node = LeafNodeMut::new(&mut right).map_err(node_error)?;
        for j in split_at..combined.len() {
            let cell = combined.entry(j).map_err(node_error)?;
            node.insert(j.saturating_sub(split_at), cell.key, cell.value)
                .map_err(node_error)?;
        }
        node.set_next_leaf(old_next).map_err(node_error)?;

        split_at
    };

    // Step 3: shrink L.
    shrink_left_leaf(&mut buffer, insertion, split_at).map_err(node_error)?;

    // Step 4: the separator is the first key of R. It is copied into the one
    // owned vector of this `put` (ADR 0005, amendment 1), because `right`
    // moves into the pending writes.
    let mut separator = Vec::new();
    separator.extend_from_slice(first_key(&right).map_err(node_error)?);

    let mut pending = Pending::default();
    let node = SplitNode {
        page: leaf_id,
        left: buffer,
        right,
    };
    propagate_split(pager, path, &mut pending, &mut separator, node)?;

    pending.write_all(pager)
}

/// Chooses the number `k` of combined entries that stay in the left leaf. It
/// is always in `1..len`, so both parts are not empty.
fn choose_split(combined: &Combined<'_>, append: bool) -> Result<usize, NodeError> {
    let last = combined.len().saturating_sub(1);
    if append {
        // Right-only append: the right leaf gets only the new cell.
        return Ok(last);
    }

    let mut total = 0usize;
    for j in 0..combined.len() {
        total = total.saturating_add(combined.entry_bytes(j)?);
    }

    // The smallest k whose left part uses at least half of the bytes. The
    // quarter rule (ADR 0005, option 6) means no single entry is more than a
    // quarter, so both parts fit.
    let mut left = 0usize;
    for j in 0..last {
        left = left.saturating_add(combined.entry_bytes(j)?);
        if left.saturating_mul(2) >= total {
            return Ok(j.saturating_add(1));
        }
    }

    Ok(last)
}

/// Removes from the leaf in `buffer` the cells that moved to the right leaf
/// and inserts the new cell if it belongs to the left part, `split_at` being
/// the number of combined entries that stay.
fn shrink_left_leaf(
    buffer: &mut [u8],
    insertion: Insertion<'_>,
    split_at: usize,
) -> Result<(), NodeError> {
    let mut leaf = LeafNodeMut::new(buffer)?;
    let count = leaf.view().cell_count();

    // The new cell is in the left part when its position is below `split_at`.
    // Then it is not in the buffer, so the left part keeps `split_at - 1` of
    // the old cells. Otherwise it keeps `split_at` old cells.
    let new_cell_stays = insertion.index < split_at;
    let keep = if new_cell_stays {
        split_at.saturating_sub(1)
    } else {
        split_at
    };
    for index in (keep..count).rev() {
        leaf.remove(index)?;
    }
    if new_cell_stays {
        leaf.insert(insertion.index, insertion.key, insertion.value)?;
    }

    Ok(())
}

/// Returns the first key of the leaf in `buffer`, borrowed from it: the
/// separator of a new right leaf.
fn first_key(buffer: &[u8]) -> Result<&[u8], NodeError> {
    LeafNode::new(buffer)?.key(0)
}

/// Gives the halves of `node`, which just split, to the tree above it
/// (*Database Internals*, ch. 4, Propagating Splits). `separator` holds the
/// key that divides the halves.
///
/// It walks the breadcrumbs from the parent of `node` up to the root. At each
/// level it inserts (left page, `separator`) in the parent at the crumb
/// position and makes the pointer after it lead to the right page. If the
/// parent has room, the walk ends. If it is full, the parent is split in
/// memory, its promoted key becomes the new `separator`, and the walk goes on
/// with the grandparent. If the crumbs run out, the node that split is the
/// root and [`place_root_split`] gives it a new root.
fn propagate_split(
    pager: &mut Pager,
    path: &Path,
    pending: &mut Pending,
    separator: &mut Vec<u8>,
    mut node: SplitNode,
) -> Result<(), BTreeError> {
    let mut remaining = path.as_slice();
    while let Some((crumb, above)) = remaining.split_last() {
        let parent_error = |source| BTreeError::Node {
            page: crumb.page,
            source,
        };

        // Check the parent before anything is allocated.
        let mut parent = page_buffer(pager);
        pager.read_page(crumb.page, &mut parent)?;
        let view = InternalNode::new(&parent).map_err(parent_error)?;
        if view.child_at(crumb.pos).map_err(parent_error)? != node.page {
            return Err(BTreeError::CorruptTree {
                page: crumb.page,
                reason: "parent does not lead to the child it was reached through",
            });
        }

        let (left_id, right_id) = place_split(pager, pending, node)?;

        // Insert (left, separator) at the position of the pointer to the left
        // page. The old pointer moves one position to the right and must lead
        // to the right page.
        let mut parent_node = InternalNodeMut::new(&mut parent).map_err(parent_error)?;
        match parent_node.insert(crumb.pos, separator, left_id) {
            Ok(()) => {
                parent_node
                    .set_child_at(crumb.pos.saturating_add(1), right_id)
                    .map_err(parent_error)?;
                pending.changed.push((crumb.page, parent));

                return Ok(());
            }
            Err(NodeError::Page(PageError::PageFull { .. })) => {}
            Err(source) => return Err(parent_error(source)),
        }

        // The parent is full: it splits too. A node is on the rightmost spine
        // when its crumb and all the crumbs above it took the last child.
        let append = crumb.last && above.iter().all(|c| c.last);
        let target = InternalTarget {
            page: crumb.page,
            buffer: &parent,
            pos: crumb.pos,
            left_child: left_id,
            right_child: right_id,
            append,
        };
        let (left, right) = split_internal(&target, separator)?;
        node = SplitNode {
            page: crumb.page,
            left,
            right,
        };
        remaining = above;
    }

    place_root_split(pager, pending, separator, node)
}

/// Gives page ids to the halves of a node that is not the root and plans their
/// writes: the left half keeps the page of the node, the right half gets a new
/// page. For leaves, the left `next_leaf` is set to the right page (the right
/// half already has the old next leaf). Returns the two page ids.
fn place_split(
    pager: &mut Pager,
    pending: &mut Pending,
    node: SplitNode,
) -> Result<(PageId, PageId), BTreeError> {
    let SplitNode {
        page,
        mut left,
        right,
    } = node;
    let right_id = pager.allocate_page()?;
    link_leaves(&mut left, right_id).map_err(|source| BTreeError::Node { page, source })?;

    pending.new_pages.push((right_id, right));
    pending.changed.push((page, left));

    Ok((page, right_id))
}

/// Plans the split of the root. The root page id never changes (ADR 0005,
/// option 4), so both halves go to new pages `a` (left) and `b` (right), and
/// the root page is rewritten as an internal node with the cell
/// (`a`, `separator`) and `b` as rightmost child. For a leaf root, `a` is
/// linked to `b`, and `b` keeps the old next leaf.
fn place_root_split(
    pager: &mut Pager,
    pending: &mut Pending,
    separator: &[u8],
    node: SplitNode,
) -> Result<(), BTreeError> {
    let SplitNode {
        page: root_id,
        mut left,
        right,
    } = node;
    let node_error = |source| BTreeError::Node {
        page: root_id,
        source,
    };

    // If a later step fails, the two pages stay allocated and unused.
    let left_id = pager.allocate_page()?;
    let right_id = pager.allocate_page()?;
    link_leaves(&mut left, right_id).map_err(node_error)?;

    let mut root = page_buffer(pager);
    InternalNodeMut::init(&mut root, right_id).map_err(node_error)?;
    InternalNodeMut::new(&mut root)
        .and_then(|mut node| node.insert(0, separator, left_id))
        .map_err(node_error)?;

    pending.new_pages.push((right_id, right));
    pending.new_pages.push((left_id, left));
    pending.changed.push((root_id, root));

    Ok(())
}

/// If `left` is a leaf, sets its `next_leaf` to `right_id`. Does nothing for an
/// internal node.
fn link_leaves(left: &mut [u8], right_id: PageId) -> Result<(), NodeError> {
    if page_type(left) != Ok(PageType::BTreeLeaf) {
        return Ok(());
    }

    LeafNodeMut::new(left)?.set_next_leaf(Some(right_id))
}

/// The full internal node that has to split, and the cell it must take.
struct InternalTarget<'a> {
    /// Page id of the node, for errors.
    page: PageId,
    /// The page of the node, unchanged.
    buffer: &'a [u8],
    /// Position where the new cell goes, from the crumb.
    pos: usize,
    /// The child that split: it is the child of the new cell.
    left_child: PageId,
    /// The new right half of that child: it replaces the old pointer, which
    /// moves to position `pos + 1`.
    right_child: PageId,
    /// `true` if the node is on the rightmost spine (right-only append).
    append: bool,
}

/// The cells of a full internal node plus the new cell, in key order, without
/// copying them. Entry `j` is read from the node or is the new cell, on
/// demand. The child pointers are shifted the same way: the new cell is
/// (`left_child`, separator) at `pos` and the pointer after it leads to
/// `right_child`.
struct CombinedInternal<'a> {
    node: InternalNode<'a>,
    pos: usize,
    separator: &'a [u8],
    left_child: PageId,
    right_child: PageId,
}

impl<'a> CombinedInternal<'a> {
    /// Number of cells: the cells of the node plus the new one.
    fn len(&self) -> usize {
        self.node.cell_count().saturating_add(1)
    }

    /// Key of cell `j`: `j < pos` is node cell `j`, `j == pos` is the new
    /// separator and `j > pos` is node cell `j - 1`.
    fn key(&self, j: usize) -> Result<&'a [u8], NodeError> {
        match j.cmp(&self.pos) {
            Ordering::Less => self.node.key(j),
            Ordering::Equal => Ok(self.separator),
            Ordering::Greater => self.node.key(j.saturating_sub(1)),
        }
    }

    /// Child at position `j`, for `j` in `0..=len`: the child of cell `j`, or
    /// the rightmost child for `j == len`.
    fn child_at(&self, j: usize) -> Result<PageId, NodeError> {
        match j.cmp(&self.pos.saturating_add(1)) {
            Ordering::Less if j == self.pos => Ok(self.left_child),
            Ordering::Less => self.node.child_at(j),
            Ordering::Equal => Ok(self.right_child),
            Ordering::Greater => self.node.child_at(j.saturating_sub(1)),
        }
    }

    /// Bytes that cell `j` uses in a page: its cell plus its slot.
    fn cell_bytes(&self, j: usize) -> Result<usize, NodeError> {
        Ok(INTERNAL_CELL_HEADER_SIZE
            .saturating_add(self.key(j)?.len())
            .saturating_add(SLOT_SIZE))
    }
}

/// Splits the full internal node `target` in memory and returns its two
/// halves, built in fresh buffers. `separator` is the new cell's key on
/// entry and the promoted key on return.
///
/// Let `n` be the number of combined cells and `m` the chosen cell. The left
/// half gets the combined cells `0..m` and, as rightmost child, the child of
/// cell `m`. The key of cell `m` is promoted: it goes up and stays in neither
/// half (ADR 0005, invariant 5). The right half gets the cells `m + 1..n` and
/// the rightmost child of the combined node.
fn split_internal(
    target: &InternalTarget<'_>,
    separator: &mut Vec<u8>,
) -> Result<(Vec<u8>, Vec<u8>), BTreeError> {
    let node_error = |source| BTreeError::Node {
        page: target.page,
        source,
    };

    let node = InternalNode::new(target.buffer).map_err(node_error)?;
    let combined = CombinedInternal {
        node,
        pos: target.pos,
        separator: separator.as_slice(),
        left_child: target.left_child,
        right_child: target.right_child,
    };
    let n = combined.len();
    if n < 3 {
        return Err(BTreeError::CorruptTree {
            page: target.page,
            reason: "an internal node that must split has fewer than two cells",
        });
    }

    let m = choose_internal_split(&combined, target.append).map_err(node_error)?;

    // Both halves are built from the virtual sequence, so every borrowed key
    // is still valid while it is copied.
    let mut left = vec![0u8; target.buffer.len()];
    let mut right = vec![0u8; target.buffer.len()];
    let left_rightmost = combined.child_at(m).map_err(node_error)?;
    fill_internal(&mut left, &combined, 0..m, left_rightmost).map_err(node_error)?;
    let right_rightmost = combined.child_at(n).map_err(node_error)?;
    let after = m.saturating_add(1);
    fill_internal(&mut right, &combined, after..n, right_rightmost).map_err(node_error)?;

    // The promoted key is either the new separator (already in the vector) or
    // a key of the node. The `combined` borrow of the vector ends here.
    if m != target.pos {
        let old_index = if m < target.pos {
            m
        } else {
            m.saturating_sub(1)
        };
        separator.clear();
        separator.extend_from_slice(node.key(old_index).map_err(node_error)?);
    }

    Ok((left, right))
}

/// Chooses the combined cell `m` that is promoted. It is always in `1..=n - 2`,
/// so both halves have at least one cell.
fn choose_internal_split(
    combined: &CombinedInternal<'_>,
    append: bool,
) -> Result<usize, NodeError> {
    let last = combined.len().saturating_sub(2);
    if append {
        // Right-only append: the right half gets only the new cell, and the
        // left half keeps the old cells but the last, which moves up.
        return Ok(last);
    }

    let mut total = 0usize;
    for j in 0..combined.len() {
        total = total.saturating_add(combined.cell_bytes(j)?);
    }

    // The smallest m whose left part (cells `0..m`) uses at least half of the
    // bytes. The quarter rule (ADR 0005, option 6) means no cell is more than
    // a quarter, so both halves fit even though the promoted cell leaves.
    let mut left = 0usize;
    for m in 1..=last {
        left = left.saturating_add(combined.cell_bytes(m.saturating_sub(1))?);
        if left.saturating_mul(2) >= total {
            return Ok(m);
        }
    }

    Ok(last)
}

/// Formats `buffer` as an internal node with `rightmost` as its rightmost
/// child and fills it with the combined cells in `cells`.
fn fill_internal(
    buffer: &mut [u8],
    combined: &CombinedInternal<'_>,
    cells: Range<usize>,
    rightmost: PageId,
) -> Result<(), NodeError> {
    InternalNodeMut::init(buffer, rightmost)?;
    let mut node = InternalNodeMut::new(buffer)?;

    let start = cells.start;
    for j in cells {
        node.insert(
            j.saturating_sub(start),
            combined.key(j)?,
            combined.child_at(j)?,
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_chain_of_an_inline_value_is_none() {
        assert_eq!(overflow_chain(ValueRef::Inline(b"abc")), None);
    }

    #[test]
    fn overflow_chain_of_an_overflow_value_is_its_length_and_first_page() {
        let value = ValueRef::Overflow {
            len: 700,
            first_page: PageId::new(5),
        };
        assert_eq!(overflow_chain(value), Some((700, PageId::new(5))));
    }

    #[test]
    fn value_len_accepts_up_to_u32_max() {
        assert_eq!(value_len_u32(0).ok(), Some(0));
        assert_eq!(
            value_len_u32(usize::try_from(u32::MAX).unwrap_or(0)).ok(),
            Some(u32::MAX)
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn value_len_rejects_more_than_u32_max() {
        let len = usize::try_from(u64::from(u32::MAX).saturating_add(1)).unwrap_or(usize::MAX);
        assert!(matches!(
            value_len_u32(len),
            Err(BTreeError::ValueTooLarge { len: l, max }) if l == len && max == 4_294_967_295
        ));
    }
}
