//! Page accounting check for the B+Tree integration tests.
//!
//! Included with `#[path = "support/leaks.rs"] mod leaks;`, next to
//! `mod btree_check;`. Every binary that includes it must use all of it.

use storage_engine_rs::storage::btree::{BTree, verify_file};
use storage_engine_rs::storage::pager::disk::Pager;

use crate::btree_check::{Entries, check_tree};

/// Checks the tree and its chains, and that every page of the file (except the
/// file header) is a node, an overflow page or a free page (`verify_file`).
/// Returns the number of overflow pages and the entries. Only for a file with
/// one tree.
pub fn ensure_no_leaks(pager: &Pager, tree: BTree) -> anyhow::Result<(usize, Entries)> {
    let stats = verify_file(pager, &[tree])?;
    Ok((stats.overflow_pages, check_tree(pager, tree)?))
}
