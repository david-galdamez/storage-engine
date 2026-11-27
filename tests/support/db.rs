//! Temp database and pager creation for the B+Tree integration tests.
//!
//! Included with `#[path = "support/db.rs"] mod db;`. Every binary that
//! includes it must use all of it (directly or through another item), or
//! `dead_code` fires, so it stays small.

use std::path::{Path, PathBuf};

use storage_engine_rs::storage::pager::disk::{Pager, PagerOptions};
use storage_engine_rs::storage::pager::types::PageSize;
use storage_engine_rs::storage::wal::log::SyncMode;
use tempfile::TempDir;

pub const PAGE_SIZE: usize = 512;

pub fn page_size() -> anyhow::Result<PageSize> {
    Ok(PageSize::new(u32::try_from(PAGE_SIZE)?)?)
}

pub fn temp_db() -> anyhow::Result<(TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("test.db");
    Ok((dir, path))
}

/// Creates a pager with fsyncs off (`SyncMode::Off`): the B+Tree tests check
/// structure, not durability, and an fsync per commit makes them slow.
pub fn new_pager(path: &Path) -> anyhow::Result<Pager> {
    let mut pager = Pager::create(
        path,
        PagerOptions {
            page_size: page_size()?,
        },
    )?;
    pager.set_sync_mode(SyncMode::Off);
    Ok(pager)
}
