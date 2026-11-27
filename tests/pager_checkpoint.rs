//! Checkpoint (ADR 0006, D8): `checkpoint`, `sync`, the threshold, the sync
//! mode and the best-effort checkpoint in `Drop`. The crash between the WAL
//! header rewrite and the truncation is covered by
//! `crashed_checkpoint_reset_replays_nothing` in `tests/wal_recovery.rs`, and
//! the poisoned `Drop` by a unit test in `src/storage/pager/disk.rs`.

use std::{fs, path::Path};

use anyhow::{Result, ensure};
use storage_engine_rs::storage::{
    pager::{
        disk::{Pager, PagerOptions},
        error::PagerError,
        page::page_lsn,
        types::{PageId, PageSize},
    },
    wal::log::{SyncMode, wal_path},
};

const PS: usize = 512;
const WAL_HEADER: u64 = 32;

fn new_pager(db: &Path) -> Result<Pager> {
    let mut pager = Pager::create(
        db,
        PagerOptions {
            page_size: PageSize::new(512)?,
        },
    )?;
    pager.set_checkpoint_threshold(u64::MAX);
    Ok(pager)
}

fn wal_len(db: &Path) -> Result<u64> {
    Ok(fs::metadata(wal_path(db))?.len())
}

/// Allocates a page and writes `fill` to it. Returns its id.
fn write_new(pager: &mut Pager, fill: u8) -> Result<PageId> {
    let id = pager.allocate_page()?;
    pager.write_page(id, &mut vec![fill; PS])?;
    Ok(id)
}

fn lsn_of(pager: &Pager, id: PageId) -> Result<u64> {
    let mut buf = vec![0u8; PS];
    pager.read_page(id, &mut buf)?;
    Ok(page_lsn(&buf)?)
}

#[test]
fn checkpoint_empties_the_wal_and_raises_base_lsn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(&db)?;
    write_new(&mut pager, 1)?;
    ensure!(wal_len(&db)? > WAL_HEADER);
    let base_before = pager.wal().header().base_lsn;
    let end_before = pager.wal().end();

    pager.checkpoint()?;

    ensure!(wal_len(&db)? == WAL_HEADER);
    ensure!(pager.wal().end() == WAL_HEADER);
    ensure!(pager.wal().header().base_lsn == base_before + end_before);
    Ok(())
}

#[test]
fn checkpoint_with_an_empty_wal_does_nothing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(&db)?;
    let base = pager.wal().header().base_lsn;
    pager.checkpoint()?;
    pager.checkpoint()?;
    ensure!(pager.wal().header().base_lsn == base);
    ensure!(wal_len(&db)? == WAL_HEADER);
    Ok(())
}

#[test]
fn lsns_keep_increasing_across_checkpoints() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(&db)?;
    let first = write_new(&mut pager, 1)?;
    let first_lsn = lsn_of(&pager, first)?;
    pager.checkpoint()?;
    let second = write_new(&mut pager, 2)?;
    let second_lsn = lsn_of(&pager, second)?;
    ensure!(second_lsn > first_lsn, "{second_lsn} <= {first_lsn}");
    pager.checkpoint()?;
    drop(pager);

    // The LSNs also keep increasing after a reopen.
    let mut pager = Pager::open(&db)?;
    let third = write_new(&mut pager, 3)?;
    ensure!(lsn_of(&pager, third)? > second_lsn);
    Ok(())
}

#[test]
fn data_survives_checkpoint_and_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(&db)?;
    let id = write_new(&mut pager, 7)?;
    pager.sync()?;
    ensure!(wal_len(&db)? == WAL_HEADER);
    drop(pager);

    let pager = Pager::open(&db)?;
    ensure!(pager.recovery_report().records == 0);
    let mut buf = vec![0u8; PS];
    pager.read_page(id, &mut buf)?;
    ensure!(buf.get(64) == Some(&7));
    Ok(())
}

#[test]
fn auto_checkpoint_keeps_the_wal_bounded() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(&db)?;
    let id = write_new(&mut pager, 0)?;
    let threshold = 4 * 1024;
    pager.set_checkpoint_threshold(threshold);
    let base = pager.wal().header().base_lsn;

    // One record is a bit over one page; the WAL never holds more than the
    // threshold plus the record that crossed it.
    let mut longest = 0;
    for i in 0..40u8 {
        pager.write_page(id, &mut vec![i; PS])?;
        longest = longest.max(wal_len(&db)?);
    }
    ensure!(longest <= threshold + 2 * u64::try_from(PS)?, "{longest}");
    ensure!(pager.wal().header().base_lsn > base, "no checkpoint ran");
    Ok(())
}

#[test]
fn threshold_max_never_checkpoints() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(&db)?;
    let id = write_new(&mut pager, 0)?;
    for i in 0..40u8 {
        pager.write_page(id, &mut vec![i; PS])?;
    }
    ensure!(pager.wal().header().base_lsn == 0);
    ensure!(wal_len(&db)? > 40 * u64::try_from(PS)?);
    Ok(())
}

#[test]
fn checkpoint_inside_atomic_is_refused() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(&db)?;
    write_new(&mut pager, 1)?;
    let len = wal_len(&db)?;

    let result: Result<(), PagerError> = pager.atomic(|p| {
        p.allocate_page()?;
        p.checkpoint()?;
        Ok(())
    });
    ensure!(matches!(result, Err(PagerError::OperationInProgress)));
    // The error aborted the operation and the WAL was not touched.
    ensure!(wal_len(&db)? == len);

    // The pager is still usable.
    pager.checkpoint()?;
    ensure!(wal_len(&db)? == WAL_HEADER);
    Ok(())
}

#[test]
fn drop_checkpoints() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(&db)?;
    write_new(&mut pager, 1)?;
    ensure!(wal_len(&db)? > WAL_HEADER);
    drop(pager);
    ensure!(wal_len(&db)? == WAL_HEADER);

    let pager = Pager::open(&db)?;
    ensure!(pager.recovery_report().records == 0);
    Ok(())
}

#[test]
fn sync_mode_off_still_checkpoints() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(&db)?;
    ensure!(pager.sync_mode() == SyncMode::Full);
    pager.set_sync_mode(SyncMode::Off);
    ensure!(pager.sync_mode() == SyncMode::Off);
    write_new(&mut pager, 1)?;
    pager.checkpoint()?;
    ensure!(wal_len(&db)? == WAL_HEADER);
    Ok(())
}
