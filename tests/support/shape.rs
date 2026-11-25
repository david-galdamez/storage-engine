//! Page buffers, tree height and sample entries for the B+Tree integration
//! tests.
//!
//! Included with `#[path = "support/shape.rs"] mod shape;`. Every binary that
//! includes it must use all of it, or `dead_code` fires.

use anyhow::Context;
use storage_engine_rs::storage::btree::internal::InternalNode;
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::types::PageId;

use crate::db::PAGE_SIZE;

pub fn blank() -> Vec<u8> {
    vec![0u8; PAGE_SIZE]
}

/// Number of levels from the root down to a leaf, following child 0.
pub fn height(pager: &Pager, root: PageId) -> anyhow::Result<usize> {
    let mut id = root;
    let mut levels = 1usize;
    loop {
        let mut buf = blank();
        pager.read_page(id, &mut buf)?;
        match InternalNode::new(&buf) {
            Ok(node) => {
                id = node.child_at(0)?;
                levels = levels.checked_add(1).context("overflow")?;
            }
            Err(_) => return Ok(levels),
        }
    }
}

pub fn numbered_entries(count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..count)
        .map(|i| {
            (
                format!("key-{:05}", i.saturating_mul(2)).into_bytes(),
                format!("value-{i}").into_bytes(),
            )
        })
        .collect()
}
