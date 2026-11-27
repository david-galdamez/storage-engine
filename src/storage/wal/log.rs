//! The WAL file: create, open, append, reset (ADR 0006).
//!
//! `lsn = base_lsn + byte offset in the file`. Appends go at `end`; a reset
//! (checkpoint) raises `base_lsn` and truncates the file back to the header.

use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

use crate::storage::{
    pager::types::PageSize,
    wal::{
        error::WalError,
        format::{HEADER_SIZE, WalHeader},
    },
};

/// Whether the WAL fsyncs. `Off` keeps the write order but skips every fsync;
/// it exists for tests only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncMode {
    /// fsync at every durability point (default).
    #[default]
    Full,
    /// Never fsync.
    Off,
}

/// Path of the WAL of database `db`: the file name with `-wal` appended.
#[must_use]
pub fn wal_path(db: &Path) -> PathBuf {
    let mut name = OsString::from(db);
    name.push("-wal");
    PathBuf::from(name)
}

/// An open WAL file.
#[derive(Debug)]
pub struct Wal {
    file: File,
    header: WalHeader,
    /// Offset where the next record is written.
    end: u64,
}

const HEADER_LEN: u64 = 32;
const _: () = assert!(HEADER_SIZE == 32);

impl Wal {
    /// Creates a new WAL at `path` with `base_lsn = 0` and fsyncs it per
    /// `sync`. The directory fsync is the caller's job (`Pager::create`).
    ///
    /// # Errors
    ///
    /// [`WalError::Io`] if the file exists or a write or sync fails.
    pub fn create(path: &Path, page_size: PageSize, sync: SyncMode) -> Result<Self, WalError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let wal = Self {
            file,
            header: WalHeader {
                page_size,
                base_lsn: 0,
            },
            end: HEADER_LEN,
        };
        wal.write_header(wal.header)?;
        wal.sync(sync)?;
        Ok(wal)
    }

    /// Opens an existing WAL and decodes its header.
    ///
    /// `end` is the file length, so appending before a reset never overwrites
    /// existing bytes. Recovery reads the log, replays it and then calls
    /// [`Wal::reset`], which brings `end` back to the header size.
    ///
    /// # Errors
    ///
    /// [`WalError::Io`], or any error of [`WalHeader::decode`].
    pub fn open(path: &Path) -> Result<Self, WalError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut buf = [0u8; HEADER_SIZE];
        file.read_exact_at(&mut buf, 0)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::UnexpectedEof => WalError::CorruptHeader,
                _ => WalError::Io(e),
            })?;
        let header = WalHeader::decode(&buf)?;
        let end = file.metadata()?.len();
        Ok(Self { file, header, end })
    }

    /// The header currently on disk.
    #[must_use]
    pub const fn header(&self) -> WalHeader {
        self.header
    }

    /// Offset where the next record goes. After [`Wal::create`] and
    /// [`Wal::reset`] it is the header size; after [`Wal::open`] it is the file length.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.end
    }

    /// LSN the next appended record must carry: `base_lsn + end`.
    #[must_use]
    pub const fn next_lsn(&self) -> u64 {
        self.header.base_lsn.saturating_add(self.end)
    }

    /// Writes an encoded record at `end`, syncs data per `sync`, advances `end`
    /// and returns the record's LSN. On error `end` does not move: the partial
    /// bytes are overwritten by the next append (ADR 0006).
    ///
    /// # Errors
    ///
    /// [`WalError::Io`] if the write or sync fails.
    pub fn append(&mut self, rec: &[u8], sync: SyncMode) -> Result<u64, WalError> {
        let lsn = self.next_lsn();
        self.file.write_all_at(rec, self.end)?;
        if sync == SyncMode::Full {
            self.file.sync_data()?;
        }
        self.end = self
            .end
            .saturating_add(u64::try_from(rec.len()).unwrap_or(u64::MAX));
        Ok(lsn)
    }

    /// Checkpoint reset (ADR 0006): rewrite the header with
    /// `base_lsn += end`, fsync, then truncate to the header. The caller must
    /// have fsynced the database file first.
    ///
    /// # Errors
    ///
    /// [`WalError::Io`] on any failure. If it fails before the truncation the
    /// old records are stale by LSN and recovery ignores them.
    pub fn reset(&mut self, sync: SyncMode) -> Result<(), WalError> {
        let header = WalHeader {
            page_size: self.header.page_size,
            base_lsn: self.next_lsn(),
        };
        self.write_header(header)?;
        self.sync(sync)?;
        self.header = header;
        self.file.set_len(HEADER_LEN)?;
        self.end = HEADER_LEN;
        Ok(())
    }

    /// Replaces `buf` with the whole file (header included), for `LogReader`.
    ///
    /// # Errors
    ///
    /// [`WalError::Io`] if reading fails.
    pub fn read_into(&self, buf: &mut Vec<u8>) -> Result<(), WalError> {
        let len =
            usize::try_from(self.file.metadata()?.len()).map_err(|_| WalError::RecordTooLarge)?;
        buf.clear();
        buf.resize(len, 0);
        self.file.read_exact_at(buf, 0)?;
        Ok(())
    }

    fn write_header(&self, header: WalHeader) -> Result<(), WalError> {
        let mut buf = [0u8; HEADER_SIZE];
        header.encode(&mut buf)?;
        self.file.write_all_at(&buf, 0)?;
        Ok(())
    }

    fn sync(&self, sync: SyncMode) -> Result<(), WalError> {
        if sync == SyncMode::Full {
            self.file.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        pager::{
            page::{set_page_lsn, stamp_checksum},
            types::PageId,
        },
        wal::format::{LogReader, encode_record},
    };
    use anyhow::{Result, ensure};

    const PS: usize = 512;

    fn ps() -> Result<PageSize> {
        Ok(PageSize::new(512)?)
    }

    /// Encodes a one-image record stamped with `lsn`.
    fn record(lsn: u64, fill: u8) -> Result<Vec<u8>> {
        let mut img = vec![fill; PS];
        set_page_lsn(&mut img, lsn)?;
        stamp_checksum(&mut img)?;
        let mut out = Vec::new();
        encode_record(
            &mut out,
            lsn,
            ps()?,
            std::iter::once((PageId::new(1), img.as_slice())),
        )?;
        Ok(out)
    }

    fn append_one(wal: &mut Wal, fill: u8, sync: SyncMode) -> Result<u64> {
        let rec = record(wal.next_lsn(), fill)?;
        Ok(wal.append(&rec, sync)?)
    }

    fn lsns(wal: &Wal) -> Result<Vec<u64>> {
        let mut buf = Vec::new();
        wal.read_into(&mut buf)?;
        Ok(LogReader::new(&buf)?.map(|r| r.lsn()).collect())
    }

    #[test]
    fn append_reopen_and_read_back() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.db-wal");
        let mut wal = Wal::create(&path, ps()?, SyncMode::Full)?;
        let a = append_one(&mut wal, 1, SyncMode::Full)?;
        let b = append_one(&mut wal, 2, SyncMode::Off)?;
        ensure!(a == HEADER_LEN);
        ensure!(b > a && wal.next_lsn() > b);
        drop(wal);

        let wal = Wal::open(&path)?;
        ensure!(wal.header().base_lsn == 0 && wal.header().page_size == ps()?);
        ensure!(lsns(&wal)? == vec![a, b]);
        ensure!(wal.end() == std::fs::metadata(&path)?.len());
        Ok(())
    }

    #[test]
    fn reset_raises_base_lsn_and_truncates() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.db-wal");
        let mut wal = Wal::create(&path, ps()?, SyncMode::Off)?;
        append_one(&mut wal, 1, SyncMode::Off)?;
        let old_end = wal.next_lsn();
        wal.reset(SyncMode::Off)?;
        ensure!(wal.header().base_lsn == old_end);
        ensure!(wal.end() == HEADER_LEN);
        ensure!(wal.next_lsn() == old_end.saturating_add(HEADER_LEN));
        ensure!(std::fs::metadata(&path)?.len() == HEADER_LEN);
        ensure!(lsns(&wal)?.is_empty());
        let next = append_one(&mut wal, 2, SyncMode::Off)?;
        drop(wal);
        ensure!(lsns(&Wal::open(&path)?)? == vec![next]);
        Ok(())
    }

    #[test]
    fn reset_without_truncation_leaves_only_stale_records() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.db-wal");
        let mut wal = Wal::create(&path, ps()?, SyncMode::Off)?;
        append_one(&mut wal, 1, SyncMode::Off)?;
        let new_base = wal.next_lsn();
        drop(wal);
        // Crash after the new header was written but before set_len.
        let mut hdr = [0u8; HEADER_SIZE];
        WalHeader {
            page_size: ps()?,
            base_lsn: new_base,
        }
        .encode(&mut hdr)?;
        let f = OpenOptions::new().write(true).open(&path)?;
        f.write_all_at(&hdr, 0)?;
        drop(f);
        ensure!(lsns(&Wal::open(&path)?)?.is_empty());
        Ok(())
    }

    #[test]
    fn create_fails_if_file_exists() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.db-wal");
        Wal::create(&path, ps()?, SyncMode::Off)?;
        ensure!(matches!(
            Wal::create(&path, ps()?, SyncMode::Off),
            Err(WalError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists
        ));
        Ok(())
    }

    #[test]
    fn open_rejects_short_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a.db-wal");
        std::fs::write(&path, b"short")?;
        ensure!(matches!(Wal::open(&path), Err(WalError::CorruptHeader)));
        Ok(())
    }

    #[test]
    fn wal_path_appends_suffix() {
        assert_eq!(wal_path(Path::new("x.db")), Path::new("x.db-wal"));
        assert_eq!(wal_path(Path::new("/a/b/x.db")), Path::new("/a/b/x.db-wal"));
        assert_eq!(wal_path(Path::new("data")), Path::new("data-wal"));
    }
}
