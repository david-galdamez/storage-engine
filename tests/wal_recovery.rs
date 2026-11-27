//! Recovery on open (ADR 0006, "Recovery"): hand-built WALs replayed by
//! `Pager::open`.

use std::{fs, os::unix::fs::FileExt, path::Path};

use anyhow::{Result, ensure};
use storage_engine_rs::storage::{
    pager::{
        disk::{Pager, PagerOptions},
        error::PagerError,
        file_header::FileHeader,
        page::{set_page_lsn, stamp_checksum},
        types::{PageId, PageSize},
    },
    wal::{
        error::WalError,
        format::{HEADER_SIZE, WalHeader, encode_record},
        log::{Wal, wal_path},
        recovery::RecoveryReport,
    },
};

const PS: usize = 512;
const _: () = assert!(HEADER_SIZE == 32);

fn ps() -> Result<PageSize> {
    Ok(PageSize::new(512)?)
}

fn new_db(path: &Path) -> Result<()> {
    Pager::create(path, PagerOptions { page_size: ps()? })?;
    Ok(())
}

/// A page image stamped with `lsn` and a valid checksum.
fn image(lsn: u64, fill: u8) -> Result<Vec<u8>> {
    let mut img = vec![fill; PS];
    set_page_lsn(&mut img, lsn)?;
    stamp_checksum(&mut img)?;
    Ok(img)
}

/// A page-0 image with `page_count` pages.
fn header_image(lsn: u64, page_count: u32) -> Result<Vec<u8>> {
    let mut img = vec![0u8; PS];
    let mut h = FileHeader::new(ps()?);
    h.page_count = page_count;
    h.encode(&mut img)?;
    set_page_lsn(&mut img, lsn)?;
    stamp_checksum(&mut img)?;
    Ok(img)
}

/// Appends one record with the given `(page id, builder(lsn))` images.
type Builder = Box<dyn Fn(u64) -> Result<Vec<u8>>>;

fn append(path: &Path, pages: &[(u32, Builder)]) -> Result<()> {
    let mut wal = Wal::open(&wal_path(path))?;
    let lsn = wal.next_lsn();
    let imgs = pages
        .iter()
        .map(|(id, f)| Ok((PageId::new(*id), f(lsn)?)))
        .collect::<Result<Vec<_>>>()?;
    let mut rec = Vec::new();
    encode_record(
        &mut rec,
        lsn,
        ps()?,
        imgs.iter().map(|(id, i)| (*id, i.as_slice())),
    )?;
    wal.append(&rec, storage_engine_rs::storage::wal::log::SyncMode::Off)?;
    Ok(())
}

fn fill(b: u8) -> Builder {
    Box::new(move |lsn| image(lsn, b))
}

fn hdr(count: u32) -> Builder {
    Box::new(move |lsn| header_image(lsn, count))
}

fn wal_len(path: &Path) -> Result<u64> {
    Ok(fs::metadata(wal_path(path))?.len())
}

#[test]
fn replays_records_and_grows_the_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("t.db");
    new_db(&path)?;
    // Growth: page 0 says 3 pages, images for pages 1 and 2. Second record
    // overwrites page 1.
    append(&path, &[(0, hdr(3)), (1, fill(1)), (2, fill(2))])?;
    append(&path, &[(1, fill(9))])?;

    let pager = Pager::open(&path)?;
    ensure!(
        pager.recovery_report()
            == RecoveryReport {
                records: 2,
                pages: 4
            }
    );
    ensure!(pager.page_count() == 3);
    ensure!(fs::metadata(&path)?.len() == 1536);
    let mut buf = vec![0u8; PS];
    pager.read_page(PageId::new(1), &mut buf)?;
    ensure!(buf.get(100) == Some(&9));
    pager.read_page(PageId::new(2), &mut buf)?;
    ensure!(buf.get(100) == Some(&2));
    // WAL reset to the header, base_lsn advanced.
    ensure!(wal_len(&path)? == 32);
    ensure!(pager.wal().header().base_lsn > 0);
    drop(pager);

    // Second open: nothing left to replay.
    let pager = Pager::open(&path)?;
    ensure!(pager.recovery_report() == RecoveryReport::default());
    Ok(())
}

#[test]
fn torn_page_zero_is_repaired_from_its_image() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("t.db");
    new_db(&path)?;
    append(&path, &[(0, hdr(1))])?;
    fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .write_all_at(&[0xAB; 200], 0)?;
    ensure!(Pager::open(&path).is_ok());
    Ok(())
}

#[test]
fn corrupt_wal_header_refuses_to_open() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("t.db");
    new_db(&path)?;
    fs::OpenOptions::new()
        .write(true)
        .open(wal_path(&path))?
        .write_all_at(&[0xFF; 4], 12)?;
    ensure!(matches!(
        Pager::open(&path),
        Err(PagerError::Wal(WalError::CorruptHeader))
    ));
    Ok(())
}

#[test]
fn wal_page_size_mismatch_is_reported() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("t.db");
    new_db(&path)?;
    let mut buf = [0u8; HEADER_SIZE];
    WalHeader {
        page_size: PageSize::new(1024)?,
        base_lsn: 0,
    }
    .encode(&mut buf)?;
    fs::OpenOptions::new()
        .write(true)
        .open(wal_path(&path))?
        .write_all_at(&buf, 0)?;
    ensure!(matches!(
        Pager::open(&path),
        Err(PagerError::Wal(WalError::PageSizeMismatch {
            wal: 1024,
            database: 512
        }))
    ));
    Ok(())
}

#[test]
fn phase2_file_without_wal_gets_one() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("t.db");
    new_db(&path)?;
    fs::remove_file(wal_path(&path))?;
    let pager = Pager::open(&path)?;
    ensure!(pager.recovery_report() == RecoveryReport::default());
    ensure!(wal_len(&path)? == 32);
    ensure!(pager.wal().header().page_size == ps()?);
    Ok(())
}

#[test]
fn create_makes_a_wal_and_refuses_a_stale_one() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("t.db");
    new_db(&path)?;
    ensure!(wal_len(&path)? == 32);

    let other = dir.path().join("u.db");
    fs::write(wal_path(&other), b"stale")?;
    ensure!(matches!(
        Pager::create(&other, PagerOptions::default()),
        Err(PagerError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists
    ));
    ensure!(
        !other.exists(),
        "nothing may be created next to a stale WAL"
    );
    Ok(())
}

#[test]
fn torn_page_zero_does_not_truncate_the_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("t.db");
    new_db(&path)?;
    append(&path, &[(0, hdr(3)), (1, fill(1)), (2, fill(2))])?;
    drop(Pager::open(&path)?);
    ensure!(fs::metadata(&path)?.len() == 1536);

    // A record that does not touch page 0, then a torn page 0 whose prefix
    // still decodes but says 1 page.
    append(&path, &[(1, fill(7))])?;
    let mut torn = header_image(0, 1)?;
    *torn
        .get_mut(PS - 1)
        .ok_or_else(|| anyhow::anyhow!("short page"))? ^= 0xFF;
    fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .write_all_at(&torn, 0)?;

    ensure!(Pager::open(&path).is_err());
    ensure!(
        fs::metadata(&path)?.len() == 1536,
        "live pages were truncated"
    );
    Ok(())
}

#[test]
fn crashed_checkpoint_reset_replays_nothing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("t.db");
    new_db(&path)?;
    append(&path, &[(0, hdr(2)), (1, fill(5))])?;

    // The reset rewrote the header with a higher base LSN, then crashed
    // before truncating: the old record is still in the file but stale.
    let wal = Wal::open(&wal_path(&path))?;
    let mut buf = [0u8; HEADER_SIZE];
    WalHeader {
        page_size: ps()?,
        base_lsn: wal.next_lsn(),
    }
    .encode(&mut buf)?;
    drop(wal);
    fs::OpenOptions::new()
        .write(true)
        .open(wal_path(&path))?
        .write_all_at(&buf, 0)?;

    let pager = Pager::open(&path)?;
    ensure!(pager.recovery_report() == RecoveryReport::default());
    ensure!(pager.page_count() == 1);
    ensure!(fs::metadata(&path)?.len() == 512);
    ensure!(wal_len(&path)? == 32);
    Ok(())
}
