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
//! This step has only [`BTree::create`] and [`BTree::open`]. Lookup, insert,
//! delete and scans come in later steps (see the Phase 2 roadmap).

use crate::storage::btree::{
    error::BTreeError,
    internal::InternalNode,
    leaf::{LeafNode, LeafNodeMut},
};
use crate::storage::pager::{
    disk::Pager,
    page::{PageType, page_type},
    types::PageId,
};

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
