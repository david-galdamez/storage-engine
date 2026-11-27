//! Crash recovery: redo of the WAL on open (ADR 0006, "Recovery").
//!
//! Every complete record is a committed operation and its images are full
//! pages, so recovery is one forward scan that writes each image to its home
//! offset. There is no analysis pass (no uncommitted records), no undo pass
//! (no-steal) and no LSN comparison (images are idempotent). Compare ARIES
//! redo in _Database Internals_, Ch. 5.

use std::{fs::File, os::unix::fs::FileExt};

use crate::storage::{
    pager::{file_header::FileHeader, page::verify_checksum, types::PageSize},
    wal::{
        error::WalError,
        format::{HEADER_SIZE, LogReader},
        log::{SyncMode, Wal},
    },
};

/// What recovery did on the last open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Valid records replayed.
    pub records: u64,
    /// Page images written to the database file.
    pub pages: u64,
}

/// Reads and decodes the file header prefix of `db`. `None` when the file is
/// too short or page 0 does not decode (it may be torn).
fn read_header(db: &File) -> Result<Option<FileHeader>, WalError> {
    let mut prefix = [0u8; 512];
    match db.read_exact_at(&mut prefix, 0) {
        Ok(()) => Ok(FileHeader::decode(&prefix).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Whether the whole page 0 of `db` has a valid checksum. The header prefix
/// alone can decode on a torn page, so `page_count` is trusted only after this.
fn page_zero_is_intact(db: &File, page_size: PageSize) -> Result<bool, WalError> {
    let mut page = vec![0u8; page_size.as_usize()];
    match db.read_exact_at(&mut page, 0) {
        Ok(()) => Ok(verify_checksum(&page).is_ok()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn check_page_size(wal: &Wal, header: &FileHeader) -> Result<(), WalError> {
    let (w, d) = (wal.header().page_size, header.page_size);
    if w == d {
        Ok(())
    } else {
        Err(WalError::PageSizeMismatch {
            wal: w.get(),
            database: d.get(),
        })
    }
}

/// Replays the valid records of `wal` into `db`, then resets the WAL.
///
/// `buf` holds the WAL bytes and is reused between calls.
///
/// Page-size check: if page 0 decodes before replay, its page size must match
/// the WAL's. If page 0 is torn it cannot be compared, and the WAL may hold
/// the image that repairs it, so the comparison is made again after replay.
///
/// Order (ADR 0006, D8/D9): write images, `set_len(page_count * page_size)`
/// from the replayed page 0, fsync `db`, then `wal.reset`. An empty WAL is
/// left alone (no replay, no fsync, no reset).
///
/// # Errors
///
/// [`WalError::PageSizeMismatch`], [`WalError::CorruptHeader`] and the other
/// header errors of [`LogReader::new`], or [`WalError::Io`]. A torn tail is
/// not an error: it ends the log.
pub fn recover(
    db: &File,
    wal: &mut Wal,
    sync: SyncMode,
    buf: &mut Vec<u8>,
) -> Result<RecoveryReport, WalError> {
    wal.read_into(buf)?;
    let mut reader = LogReader::new(buf)?;
    let page_size = reader.header().page_size;

    if let Some(header) = read_header(db)? {
        check_page_size(wal, &header)?;
    }

    let mut report = RecoveryReport::default();
    for record in reader.by_ref() {
        for (id, image) in record.pages() {
            db.write_all_at(image, id.offset(page_size))?;
            report.pages = report.pages.saturating_add(1);
        }
        report.records = report.records.saturating_add(1);
    }

    if report.records > 0 {
        // Page 0 was rewritten if the WAL held its image; either way it now
        // says how long the file must be (a record may have grown it). A torn
        // page 0 is left alone: its page_count could truncate live pages, and
        // the open checks report it as corrupted.
        if let Some(header) = read_header(db)? {
            check_page_size(wal, &header)?;
            if page_zero_is_intact(db, page_size)? {
                let len = u64::from(header.page_count).saturating_mul(u64::from(page_size.get()));
                db.set_len(len)?;
            }
        }
        if sync == SyncMode::Full {
            db.sync_all()?;
        }
    }

    // shortcut: an already-empty WAL is not reset, to avoid an fsync per open.
    if wal.end() > u64::try_from(HEADER_SIZE).unwrap_or(u64::MAX) {
        wal.reset(sync)?;
    }
    Ok(report)
}
