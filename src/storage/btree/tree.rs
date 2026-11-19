//! The B+Tree handle.
//!
//! A [`BTree`] is a small `Copy` value that stores only the page id of the
//! root of one tree. The root page id never changes while the tree lives
//! (ADR 0005, option 4: a root split moves the old root's contents to a new
//! page), so it is also the id of the tree and the catalog can store it.
//!
//! The handle does not own the pager. Every operation takes the pager as an
//! argument: `&Pager` for reads and `&mut Pager` for writes. Many trees share
//! one pager, and the borrow checker keeps writes and reads apart.
//!
//! This step has [`BTree::create`], [`BTree::open`], [`BTree::get`] and
//! [`BTree::put`]. `put` splits a full leaf (including the root leaf, which
//! keeps its page id). It does not split internal nodes yet: when the parent
//! has no room for the new separator it fails with
//! [`BTreeError::Unsupported`] and changes nothing. Delete and scans come in
//! later steps (see the Phase 2 roadmap).

use std::cmp::Ordering;

use crate::storage::btree::{
    error::{BTreeError, NodeError},
    internal::{InternalNode, InternalNodeMut},
    leaf::{LeafNode, LeafNodeMut},
    node::{LeafCell, ValueRef, fits_inline, leaf_cell_len, max_key_len},
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
const MAX_TREE_HEIGHT: usize = 33;

/// One step of a descent: an internal node and the child position taken in it.
#[derive(Debug, Clone, Copy)]
struct Crumb {
    /// The internal node that was visited.
    page: PageId,
    /// The child position taken in it, in `0..=N` (see
    /// [`InternalNode::child_for`]).
    pos: usize,
}

/// The breadcrumbs of a descent: the internal nodes from the root down to the
/// parent of the leaf, with the child position taken in each (*Database
/// Internals*, ch. 4, Breadcrumbs).
///
/// It is filled by `BTree::find_leaf` and has no heap allocation: a fixed array
/// of [`MAX_TREE_HEIGHT`] crumbs is enough, because a descent never visits
/// more levels. The path is empty when the root is the leaf. A leaf split reads
/// the last crumb to find the parent that receives the separator. Step 7 (the
/// internal split) will walk the crumbs upward to propagate a split to the
/// grandparent and above.
#[derive(Debug)]
struct Path {
    crumbs: [Crumb; MAX_TREE_HEIGHT],
    len: usize,
}

impl Path {
    /// Creates an empty path. `filler` only fills the unused array slots.
    const fn new(filler: PageId) -> Self {
        Self {
            crumbs: [Crumb {
                page: filler,
                pos: 0,
            }; MAX_TREE_HEIGHT],
            len: 0,
        }
    }

    /// Removes all the crumbs.
    const fn clear(&mut self) {
        self.len = 0;
    }

    /// Appends a crumb at the bottom of the path.
    fn push(&mut self, crumb: Crumb) -> Result<(), BTreeError> {
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

    /// Returns the crumb closest to the leaf: the parent of the leaf. `None`
    /// if the leaf is the root.
    fn last(&self) -> Option<Crumb> {
        let last = self.len.checked_sub(1)?;
        self.crumbs.get(last).copied()
    }
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
    /// and returns `true`. If it is not found, returns `false` and leaves
    /// `out` untouched.
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
    /// - [`BTreeError::Unsupported`] if the value is stored in overflow pages
    ///   (temporary, until Phase 2 step 8).
    pub fn get(self, pager: &Pager, key: &[u8], out: &mut Vec<u8>) -> Result<bool, BTreeError> {
        let mut buffer = page_buffer(pager);
        // `get` does not need the breadcrumbs.
        let mut path = Path::new(self.root);
        let leaf_id = self.find_leaf(pager, key, &mut buffer, &mut path)?;

        let node_error = |source| BTreeError::Node {
            page: leaf_id,
            source,
        };
        let leaf = LeafNode::new(&buffer).map_err(node_error)?;
        let Ok(index) = leaf.search(key).map_err(node_error)? else {
            return Ok(false);
        };

        match leaf.cell(index).map_err(node_error)?.value {
            ValueRef::Inline(value) => {
                out.clear();
                out.extend_from_slice(value);
                Ok(true)
            }
            ValueRef::Overflow { .. } => Err(BTreeError::Unsupported {
                what: "overflow values (Phase 2, step 8)",
            }),
        }
    }

    /// Inserts `key` with `value`, or replaces the value if `key` exists
    /// (upsert). Returns `true` if a value was replaced and `false` if the key
    /// is new.
    ///
    /// The descent is the one of [`BTree::get`] and works for a tree of any
    /// height. If the leaf has room, the change is made in the leaf's page
    /// buffer and the page is written once, only after the change succeeded.
    ///
    /// If the leaf is full it is split (ADR 0005, invariant 5). The root leaf
    /// is split in a way that keeps the root page id. When the parent has no
    /// room for the new separator, the parent would have to be split too:
    /// that is Phase 2 step 7, so `put` returns [`BTreeError::Unsupported`]
    /// before it allocates or writes anything.
    ///
    /// A split writes several pages and there is no WAL yet (Phase 3). A
    /// crash or an I/O error between the writes can leave the tree
    /// inconsistent and allocated pages unused. The pages are written in an
    /// order that keeps the tree readable as long as possible: new pages
    /// first, then the split leaf, then the parent. Does not sync.
    ///
    /// # Errors
    ///
    /// - [`BTreeError::KeyTooLarge`] if `key` is longer than `max_key_len`.
    /// - [`BTreeError::Unsupported`] if the value does not fit inline
    ///   (overflow values, Phase 2 step 8), if the old value is in overflow
    ///   pages, or if the leaf is full and its parent has no room for the
    ///   separator (internal node splits, Phase 2 step 7). In all these cases
    ///   the file is unchanged.
    /// - [`BTreeError::Pager`] if a read, an allocation or a write fails or a
    ///   checksum is wrong. If this happens during a split, pages may already
    ///   be allocated or written.
    /// - [`BTreeError::Node`] if a node on the path does not decode.
    /// - [`BTreeError::CorruptTree`] if the root or a child is not a B+Tree
    ///   node, the descent is deeper than the maximum height, or a leaf that
    ///   must split has fewer than two entries.
    pub fn put(self, pager: &mut Pager, key: &[u8], value: &[u8]) -> Result<bool, BTreeError> {
        let page_size = pager.page_size();

        let max = max_key_len(page_size);
        if key.len() > max {
            return Err(BTreeError::KeyTooLarge {
                len: key.len(),
                max,
            });
        }
        if !fits_inline(page_size, key.len(), value.len()) {
            return Err(BTreeError::Unsupported {
                what: "overflow values (Phase 2, step 8)",
            });
        }

        let mut buffer = page_buffer(pager);
        let mut path = Path::new(self.root);
        let leaf_id = self.find_leaf(pager, key, &mut buffer, &mut path)?;

        let node_error = |source| BTreeError::Node {
            page: leaf_id,
            source,
        };
        // The read view's borrow of `buffer` ends after its last use, so the
        // mutable view below can take the buffer afterwards.
        let leaf = LeafNode::new(&buffer).map_err(node_error)?;
        let (index, replaced) = match leaf.search(key).map_err(node_error)? {
            Ok(index) => {
                let old = leaf.cell(index).map_err(node_error)?;
                if matches!(old.value, ValueRef::Overflow { .. }) {
                    return Err(BTreeError::Unsupported {
                        what: "replacing overflow values (Phase 2, step 8)",
                    });
                }
                (index, true)
            }
            Err(index) => (index, false),
        };

        let mut leaf = LeafNodeMut::new(&mut buffer).map_err(node_error)?;
        if replaced {
            leaf.remove(index).map_err(node_error)?;
        }
        let value = ValueRef::Inline(value);
        match leaf.insert(index, key, value) {
            Ok(()) => {
                pager.write_page(leaf_id, &mut buffer)?;
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
        split_leaf(pager, &path, leaf_id, &mut buffer, insertion)?;

        Ok(replaced)
    }

    /// Descends from the root to the leaf whose subtree can hold `key`
    /// (ADR 0005, invariant 4). Returns the id of the leaf, whose page is left
    /// in `buffer`. All levels read into the same `buffer`, which must be one
    /// page long.
    ///
    /// `path` is cleared and then receives one crumb for each internal node on
    /// the way down, with the child position taken. It stays empty when the
    /// root is the leaf.
    fn find_leaf(
        self,
        pager: &Pager,
        key: &[u8],
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
                    let (pos, child) = node.child_for(key).map_err(node_error)?;
                    path.push(Crumb { page: current, pos })?;
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

/// Allocates a zeroed buffer of one page, owned by the caller (ADR 0005,
/// option 7, until the buffer pool exists).
fn page_buffer(pager: &Pager) -> Vec<u8> {
    vec![0u8; pager.page_size().as_usize()]
}

/// Splits the full leaf `leaf_id`, whose page is in `buffer`, to make room for
/// `insertion`, and puts the separator in the parent (ADR 0005, invariants 4
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
/// 4. The separator is the first key of `R`, copied in full into the parent.
///    - `L` has a parent: insert the cell (`L`, separator) before the pointer
///      that led to `L`, and make that pointer lead to `R`. `L` keeps the keys
///      below the separator and `R` the keys from it up (invariant 4).
///    - `L` is the root: its page id must not change (ADR 0005, option 4). The
///      left part moves to a new page, the right part goes to another new page
///      and the root page becomes an internal node over the two.
///
/// No WAL exists yet, so the writes are ordered to do the least damage if
/// something fails in between: new pages first, then `L`, and the parent
/// last. A failure after an allocation can leave unused pages in the file,
/// and a crash between the writes can leave the tree inconsistent until
/// Phase 3.
///
/// # Errors
///
/// - [`BTreeError::Unsupported`] if the parent has no room for the separator
///   (Phase 2 step 7). This is checked in memory before any allocation, so
///   the file is unchanged.
/// - [`BTreeError::Pager`] if a read, an allocation or a write fails.
/// - [`BTreeError::Node`] if a node does not decode or a part does not fit in
///   its page, which the size limits (ADR 0005, option 6) rule out in a valid
///   tree.
/// - [`BTreeError::CorruptTree`] if the leaf has fewer than two entries with
///   the new one, or the parent does not point to the leaf.
fn split_leaf(
    pager: &mut Pager,
    path: &Path,
    leaf_id: PageId,
    buffer: &mut [u8],
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
        let leaf = LeafNode::new(buffer).map_err(node_error)?;
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
    shrink_left_leaf(buffer, insertion, split_at).map_err(node_error)?;

    // Step 4: the separator is the first key of R. Each finisher below reads
    // it from the `right` buffer and copies it into the parent.
    match path.last() {
        Some(parent) => split_with_parent(pager, leaf_id, buffer, &mut right, parent),
        None => split_root_leaf(pager, leaf_id, buffer, &mut right),
    }
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

/// Finishes the split of a leaf that has a parent: `parent` is the last crumb
/// of the descent.
fn split_with_parent(
    pager: &mut Pager,
    leaf_id: PageId,
    left: &mut [u8],
    right: &mut [u8],
    parent: Crumb,
) -> Result<(), BTreeError> {
    let parent_error = |source| BTreeError::Node {
        page: parent.page,
        source,
    };

    let separator = first_key(right).map_err(|source| BTreeError::Node {
        page: leaf_id,
        source,
    })?;

    let mut parent_buffer = page_buffer(pager);
    pager.read_page(parent.page, &mut parent_buffer)?;

    // The parent must lead to L through the position the descent took.
    let view = InternalNode::new(&parent_buffer).map_err(parent_error)?;
    if view.child_at(parent.pos).map_err(parent_error)? != leaf_id {
        return Err(BTreeError::CorruptTree {
            page: parent.page,
            reason: "parent does not lead to the leaf it was reached through",
        });
    }

    // Insert (L, separator) at the position of the pointer to L. The old
    // pointer to L moves one position to the right and will lead to R. This
    // is done in memory before any allocation, so a full parent changes
    // nothing in the file.
    let mut node = InternalNodeMut::new(&mut parent_buffer).map_err(parent_error)?;
    match node.insert(parent.pos, separator, leaf_id) {
        Ok(()) => {}
        Err(NodeError::Page(PageError::PageFull { .. })) => {
            return Err(BTreeError::Unsupported {
                what: "internal node splits (Phase 2, step 7)",
            });
        }
        Err(source) => return Err(parent_error(source)),
    }

    // From here on the file may change.
    let right_id = pager.allocate_page()?;
    node.set_child_at(parent.pos.saturating_add(1), right_id)
        .map_err(parent_error)?;

    // L -> R -> (old next of L). R already has the old next of L.
    LeafNodeMut::new(left)
        .and_then(|mut leaf| leaf.set_next_leaf(Some(right_id)))
        .map_err(|source| BTreeError::Node {
            page: leaf_id,
            source,
        })?;

    // New page first, then L, the parent last.
    pager.write_page(right_id, right)?;
    pager.write_page(leaf_id, left)?;
    pager.write_page(parent.page, &mut parent_buffer)?;

    Ok(())
}

/// Finishes the split of the root leaf. The root page id never changes (ADR
/// 0005, option 4), so both parts go to new pages `a` (left) and `b` (right),
/// and the root page is rewritten as an internal node with the cell
/// (`a`, separator) and `b` as rightmost child.
fn split_root_leaf(
    pager: &mut Pager,
    root_id: PageId,
    left: &mut [u8],
    right: &mut [u8],
) -> Result<(), BTreeError> {
    let node_error = |source| BTreeError::Node {
        page: root_id,
        source,
    };
    let separator = first_key(right).map_err(node_error)?;

    // The file may change from here on. If a later step fails, the two pages
    // stay allocated and unused.
    let left_id = pager.allocate_page()?;
    let right_id = pager.allocate_page()?;

    // The new root is built in its own buffer, because `left` becomes page a.
    let mut root = page_buffer(pager);
    InternalNodeMut::init(&mut root, right_id).map_err(node_error)?;
    InternalNodeMut::new(&mut root)
        .and_then(|mut node| node.insert(0, separator, left_id))
        .map_err(node_error)?;

    // a -> b. b already has the old next of the root leaf.
    LeafNodeMut::new(left)
        .and_then(|mut leaf| leaf.set_next_leaf(Some(right_id)))
        .map_err(node_error)?;

    // The two new pages first, the root last.
    pager.write_page(right_id, right)?;
    pager.write_page(left_id, left)?;
    pager.write_page(root_id, &mut root)?;

    Ok(())
}
