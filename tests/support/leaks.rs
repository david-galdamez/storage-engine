//! Page accounting check for the B+Tree integration tests.
//!
//! Included with `#[path = "support/leaks.rs"] mod leaks;`, next to
//! `mod btree_check;`. Every binary that includes it must use all of it.

use anyhow::{Context, ensure};
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::pager::disk::Pager;

use crate::btree_check::{Entries, check_tree_pages};

/// Checks the tree and its chains, and that every page of the file (except the
/// file header) is a node, an overflow page or a free page. Returns the
/// number of overflow pages and the entries. Only for a file with one tree.
pub fn ensure_no_leaks(pager: &Pager, tree: BTree) -> anyhow::Result<(usize, Entries)> {
    let (entries, nodes, overflow) = check_tree_pages(pager, tree)?;
    pager.verify_freelist()?;
    let used = usize::try_from(pager.page_count())?
        .checked_sub(1)
        .context("no pages")?;
    let free = usize::try_from(pager.free_page_count())?;
    let accounted = nodes
        .checked_add(overflow)
        .and_then(|n| n.checked_add(free))
        .context("overflow")?;
    ensure!(
        used == accounted,
        "page accounting: {used} pages, {nodes} nodes + {overflow} overflow + {free} free"
    );
    Ok((overflow, entries))
}
