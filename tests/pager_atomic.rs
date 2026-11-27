//! Logged writes (ADR 0006, D4/D5/D7): `Pager::atomic`, autocommit,
//! read-your-writes, deferred growth, page LSN stamping and commit.

use std::{fs, path::Path};

use anyhow::{Result, anyhow, ensure};
use storage_engine_rs::storage::{
    pager::{
        disk::{Pager, PagerOptions},
        error::PagerError,
        page::page_lsn,
        types::{PageId, PageSize},
    },
    wal::{format::LogReader, log::wal_path},
};

const PS: usize = 512;

fn new_pager(dir: &Path, extra: u32) -> Result<Pager> {
    let mut pager = Pager::create(
        &dir.join("t.db"),
        PagerOptions {
            page_size: PageSize::new(512)?,
        },
    )?;
    for _ in 0..extra {
        pager.allocate_page()?;
    }
    Ok(pager)
}

fn page(fill: u8) -> Vec<u8> {
    vec![fill; PS]
}

fn read(pager: &Pager, id: u32) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; PS];
    pager.read_page(PageId::new(id), &mut buf)?;
    Ok(buf)
}

/// Number of valid records in the WAL file of the database at `db`.
fn wal_records(db: &Path) -> Result<usize> {
    let bytes = fs::read(wal_path(db))?;
    Ok(LogReader::new(&bytes)?.count())
}

#[test]
fn failed_atomic_changes_nothing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(dir.path(), 3)?;
    pager.write_page(PageId::new(1), &mut page(1))?;
    pager.free_page(PageId::new(3))?;

    let bytes = fs::read(&db)?;
    let first_page = read(&pager, 1)?;
    let (count, free, wal_end) = (
        pager.page_count(),
        pager.free_page_count(),
        pager.wal().end(),
    );

    let result = pager.atomic(|p| -> Result<()> {
        p.write_page(PageId::new(1), &mut page(9))?;
        p.allocate_page()?; // pops page 3
        p.allocate_page()?; // grows
        p.free_page(PageId::new(2))?;
        p.set_catalog_root(Some(PageId::new(1)))?;
        Err(anyhow!("boom"))
    });
    ensure!(result.is_err());

    ensure!(fs::read(&db)? == bytes);
    ensure!(pager.page_count() == count);
    ensure!(pager.free_page_count() == free);
    ensure!(pager.catalog_root().is_none());
    ensure!(pager.wal().end() == wal_end);
    ensure!(read(&pager, 1)? == first_page);
    pager.verify_freelist()?;

    drop(pager);
    let pager = Pager::open(&db)?;
    ensure!(fs::read(&db)? == bytes);
    ensure!(pager.page_count() == count && pager.free_page_count() == free);
    pager.verify_freelist()?;
    Ok(())
}

#[test]
fn page_lsn_is_stamped_per_operation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(dir.path(), 1)?;

    let expected = pager.wal().next_lsn();
    let mut buf = page(5);
    pager.write_page(PageId::new(1), &mut buf)?;
    ensure!(page_lsn(&buf)? == expected, "buffer must be stamped");
    ensure!(page_lsn(&read(&pager, 1)?)? == expected);

    let bytes = fs::read(wal_path(&db))?;
    let last = LogReader::new(&bytes)?
        .last()
        .ok_or_else(|| anyhow!("no record"))?;
    ensure!(last.lsn() == expected);
    ensure!(last.pages().all(|(_, img)| page_lsn(img) == Ok(expected)));
    Ok(())
}

#[test]
fn read_your_writes_and_files_untouched_until_commit() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(dir.path(), 1)?;
    let before = fs::read(&db)?;

    pager.atomic(|p| -> Result<()> {
        let mut buf = page(7);
        p.write_page(PageId::new(1), &mut buf)?;
        let mut got = vec![0u8; PS];
        p.read_page(PageId::new(1), &mut got)?;
        ensure!(got == buf);
        ensure!(fs::read(&db)? == before, "nothing may reach the file yet");
        Ok(())
    })?;

    ensure!(fs::read(&db)? != before);
    ensure!(read(&pager, 1)?.first() != Some(&0));
    Ok(())
}

#[test]
fn free_then_allocate_in_one_op_reuses_the_page() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut pager = new_pager(dir.path(), 2)?;
    let count = pager.page_count();

    let id = pager.atomic(|p| -> Result<PageId> {
        p.free_page(PageId::new(2))?;
        Ok(p.allocate_page()?)
    })?;
    ensure!(id == PageId::new(2));
    ensure!(pager.page_count() == count);
    ensure!(pager.free_page_count() == 0);
    pager.verify_freelist()?;
    Ok(())
}

#[test]
fn growth_reaches_the_file_only_at_commit() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(dir.path(), 1)?;
    let len = fs::metadata(&db)?.len();

    let aborted = pager.atomic(|p| -> Result<()> {
        let id = p.allocate_page()?;
        ensure!(fs::metadata(&db)?.len() == len);
        // Never written: reads like today, a checksum failure.
        let err = p.read_page(id, &mut vec![0u8; PS]);
        ensure!(matches!(err, Err(PagerError::Corrupted { .. })));
        Err(anyhow!("abort"))
    });
    ensure!(aborted.is_err());
    ensure!(fs::metadata(&db)?.len() == len);
    ensure!(pager.page_count() == 2);

    pager.atomic(|p| -> Result<()> {
        p.allocate_page()?;
        ensure!(fs::metadata(&db)?.len() == len);
        Ok(())
    })?;
    ensure!(fs::metadata(&db)?.len() == len + 512);
    Ok(())
}

#[test]
fn committed_op_survives_without_checkpoint() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(dir.path(), 2)?;
    pager.atomic(|p| -> Result<()> {
        p.write_page(PageId::new(1), &mut page(0xA1))?;
        p.write_page(PageId::new(2), &mut page(0xB2))?;
        Ok(())
    })?;
    let want1 = read(&pager, 1)?;
    let want2 = read(&pager, 2)?;

    // Copy the files while the pager is alive: no checkpoint has run.
    let copy_dir = tempfile::tempdir()?;
    let copy = copy_dir.path().join("c.db");
    fs::copy(&db, &copy)?;
    fs::copy(wal_path(&db), wal_path(&copy))?;
    // Lose the applied pages and the grown tail.
    let mut bytes = fs::read(&copy)?;
    bytes.truncate(PS);
    fs::write(&copy, &bytes)?;

    let reopened = Pager::open(&copy)?;
    ensure!(reopened.recovery_report().records >= 1);
    ensure!(read(&reopened, 1)? == want1);
    ensure!(read(&reopened, 2)? == want2);
    Ok(())
}

#[test]
fn nested_atomic_joins_one_record() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(dir.path(), 2)?;
    let before = wal_records(&db)?;

    pager.atomic(|p| -> Result<()> {
        p.write_page(PageId::new(1), &mut page(1))?;
        p.atomic(|q| -> Result<()> {
            q.write_page(PageId::new(2), &mut page(2))?;
            Ok(())
        })?;
        p.allocate_page()?;
        Ok(())
    })?;
    ensure!(wal_records(&db)? == before + 1);
    Ok(())
}

#[test]
fn op_without_changes_writes_no_record() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut pager = new_pager(dir.path(), 1)?;
    let end = pager.wal().end();
    pager.atomic(|p| -> Result<()> {
        p.read_page(PageId::new(0), &mut vec![0u8; PS])?;
        Ok(())
    })?;
    ensure!(pager.wal().end() == end);
    Ok(())
}

#[test]
fn autocommit_writes_one_record_per_call() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("t.db");
    let mut pager = new_pager(dir.path(), 1)?;
    let before = wal_records(&db)?;
    pager.write_page(PageId::new(1), &mut page(3))?;
    pager.set_catalog_root(Some(PageId::new(1)))?;
    pager.allocate_page()?;
    ensure!(wal_records(&db)? == before + 3);
    Ok(())
}
