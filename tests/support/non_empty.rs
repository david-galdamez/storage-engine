//! Fill check for the B+Tree integration tests (ADR 0005, invariant 8).
//!
//! Included with `#[path = "support/non_empty.rs"] mod non_empty;`. Every
//! binary that includes it must use all of it, or `dead_code` fires. It is a
//! separate module, and not part of `btree_check`, because the other binaries
//! that include `btree_check` would not use it.

use anyhow::ensure;
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::btree::internal::InternalNode;
use storage_engine_rs::storage::btree::leaf::LeafNode;
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::types::PageId;

/// A descent deeper than this is a cycle, not a tree.
const MAX_DEPTH: usize = 40;

/// Checks that no non-root leaf is empty and every internal node has at least
/// one cell. Only the root, when it is a leaf, may be empty. Does not check
/// anything else: use `check_tree` for that.
pub fn check_non_empty(pager: &Pager, tree: BTree) -> anyhow::Result<()> {
    visit(pager, tree.root(), true, 0)
}

fn visit(pager: &Pager, id: PageId, is_root: bool, depth: usize) -> anyhow::Result<()> {
    ensure!(depth < MAX_DEPTH, "page {id}: tree is too deep");
    let mut buf = vec![0u8; pager.page_size().as_usize()];
    pager.read_page(id, &mut buf)?;

    if let Ok(node) = InternalNode::new(&buf) {
        let count = node.cell_count();
        ensure!(count >= 1, "internal node {id} has no cells");
        for pos in 0..=count {
            let next = depth.saturating_add(1);
            visit(pager, node.child_at(pos)?, false, next)?;
        }
        return Ok(());
    }

    let leaf = LeafNode::new(&buf)?;
    ensure!(
        is_root || leaf.cell_count() > 0,
        "non-root leaf {id} is empty"
    );
    Ok(())
}
