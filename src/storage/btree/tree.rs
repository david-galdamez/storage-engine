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
//! This step has [`BTree::create`], [`BTree::open`] and [`BTree::get`]. Insert,
//! delete and scans come in later steps (see the Phase 2 roadmap).

use crate::storage::btree::{
    error::BTreeError,
    internal::InternalNode,
    leaf::{LeafNode, LeafNodeMut},
    node::ValueRef,
};
use crate::storage::pager::{
    disk::Pager,
    page::{PageType, page_type},
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
        let leaf_id = self.find_leaf(pager, key, &mut buffer)?;

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

    /// Descends from the root to the leaf whose subtree can hold `key`
    /// (ADR 0005, invariant 4). Returns the id of the leaf, whose page is left
    /// in `buffer`. All levels read into the same `buffer`, which must be one
    /// page long.
    fn find_leaf(self, pager: &Pager, key: &[u8], buffer: &mut [u8]) -> Result<PageId, BTreeError> {
        let mut current = self.root;

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
                    current = node.child_for(key).map_err(node_error)?.1;
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
