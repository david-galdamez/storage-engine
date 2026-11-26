//! Lookup helpers and assertions for the B+Tree integration tests.
//!
//! Included with `#[path = "support/lookup.rs"] mod lookup;`. Every binary
//! that includes it must use all of it, or `dead_code` fires.

use anyhow::ensure;
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::pager::disk::Pager;

pub fn get(tree: BTree, pager: &Pager, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    Ok(tree.get(pager, key, &mut out)?.then_some(out))
}

pub fn ensure_value(got: Option<&[u8]>, expected: &[u8], what: &str) -> anyhow::Result<()> {
    ensure!(
        got == Some(expected),
        "{what}: expected {expected:?}, got {got:?}"
    );
    Ok(())
}

pub fn ensure_missing(got: Option<&[u8]>, what: &str) -> anyhow::Result<()> {
    ensure!(got.is_none(), "{what}: expected a miss, got {got:?}");
    Ok(())
}
