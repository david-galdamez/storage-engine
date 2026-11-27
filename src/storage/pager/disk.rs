//! Disk I/O: reading and writing pages at `page_id * page_size` offsets.

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    os::unix::fs::FileExt,
    path::Path,
};

use crate::storage::{
    pager::{
        error::PagerError,
        file_header::FileHeader,
        freelist::{decode_free_page, encode_free_page},
        op::OpState,
        page::{set_page_lsn, stamp_checksum, verify_checksum},
        types::{PageId, PageSize},
    },
    wal::{
        error::WalError,
        format::encode_record,
        log::{SyncMode, Wal, wal_path},
        recovery::{RecoveryReport, recover},
    },
};

/// Options for creating a new database file with [`Pager::create`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PagerOptions {
    /// Size of every page in the file. Fixed for the life of the file: it is
    /// stored in the file header (ADR 0001). Defaults to 4 KiB.
    pub page_size: PageSize,
}

/// Owns one database file and reads and writes it one fixed-size page at a time.
///
/// Page N lives at byte offset `N * page_size`. Page 0 holds the file header
/// and is managed by the pager itself. The pager stamps the checksum of every
/// page it writes and verifies it on every page it reads. It does not cache
/// pages: every call goes to the file.
///
/// Every change runs inside an atomic operation (ADR 0006): the changed pages
/// are held in memory and, at commit, written to the WAL as one record and then
/// to the file. Public writers called outside [`Pager::atomic`] run as their
/// own one-call operation (autocommit).
#[derive(Debug)]
pub struct Pager {
    /// The database file, open for reading and writing.
    file: File,
    /// In-memory copy of the file header. Always matches page 0 on disk.
    file_header: FileHeader,
    /// One page of reusable memory for writing page 0, so updating the header
    /// does not allocate.
    scratch: Box<[u8]>,
    /// The write-ahead log (ADR 0006). Empty in normal use until step 4.
    wal: Wal,
    /// Whether fsyncs happen. Always `Full` until step 5 adds a setter.
    sync_mode: SyncMode,
    /// What recovery did when this pager was opened.
    recovery: RecoveryReport,
    /// The operation in progress, if any.
    op: Option<OpState>,
    /// Buffer for the encoded WAL record, reused across operations.
    wal_buf: Vec<u8>,
    /// Set when a commit failed at or after the WAL fsync. Every later call
    /// fails with [`PagerError::Poisoned`].
    poisoned: bool,
}

impl Pager {
    /// Creates a new database file at `path` that holds only page 0 (the file
    /// header), then syncs it to disk.
    ///
    /// # Errors
    ///
    /// - [`PagerError::Io`] if the file already exists
    ///   ([`std::io::ErrorKind::AlreadyExists`]), or if creating it, writing
    ///   page 0, or syncing fails. Also [`std::io::ErrorKind::AlreadyExists`]
    ///   if a `-wal` file is already there: a stale log must never be replayed
    ///   into a new database, so it is refused before anything is created.
    /// - [`PagerError::Wal`] if creating the WAL fails.
    pub fn create(path: &Path, pager_option: PagerOptions) -> Result<Self, PagerError> {
        let wal_file = wal_path(path);
        if wal_file.try_exists()? {
            return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists).into());
        }
        let file = File::create_new(path)?;
        let sync_mode = SyncMode::Full;

        let file_header = FileHeader::new(pager_option.page_size);

        // Write and fsync page 0 first, then the WAL, then the directory so
        // both new entries survive a crash (ADR 0006, "Creating a database").
        let mut scratch = vec![0u8; pager_option.page_size.as_usize()].into_boxed_slice();
        file_header.encode(&mut scratch)?;
        stamp_checksum(&mut scratch)?;
        file.write_all_at(&scratch, 0)?;
        file.sync_all()?;

        let wal = Wal::create(&wal_file, pager_option.page_size, sync_mode)?;
        sync_parent_dir(path)?;

        Ok(Self {
            file,
            file_header,
            scratch,
            wal,
            sync_mode,
            recovery: RecoveryReport::default(),
            op: None,
            wal_buf: Vec::new(),
            poisoned: false,
        })
    }

    /// Opens an existing database file at `path` and validates it: the file
    /// header, the file length, and the checksum of page 0. The page size
    /// comes from the file header, not from the caller.
    ///
    /// # Errors
    ///
    /// The checks run in this order, and the first one that fails is returned:
    ///
    /// - [`PagerError::Wal`] if the WAL header is corrupt or its page size
    ///   differs from the database's. Checked first, during recovery.
    /// - [`PagerError::Io`] if the file does not exist, cannot be opened for
    ///   reading and writing, or a read fails.
    /// - [`PagerError::FileTooSmall`] if the file is shorter than 512 bytes.
    /// - [`PagerError::Corrupted`] with page 0 if the file header does not
    ///   decode (bad magic, unsupported version, invalid page size, or
    ///   contradictory fields).
    /// - [`PagerError::FileSizeMismatch`] if the file length is not
    ///   `page_count * page_size`.
    /// - [`PagerError::Corrupted`] with page 0 and
    ///   [`PageError::ChecksumMismatch`](crate::storage::pager::error::PageError::ChecksumMismatch)
    ///   if the checksum of the whole page 0 is wrong.
    pub fn open(path: &Path) -> Result<Self, PagerError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let sync_mode = SyncMode::Full;

        // Recovery runs before the checks below: replay may repair a torn
        // page 0 or fix the file length (ADR 0006, "Recovery").
        let mut wal = open_or_create_wal(&file, path, sync_mode)?;
        let mut wal_buf = Vec::new();
        let recovery = recover(&file, &mut wal, sync_mode, &mut wal_buf)?;

        let file_len = file.metadata()?.len();
        if file_len < u64::from(PageSize::MIN) {
            return Err(PagerError::FileTooSmall { len: file_len });
        }

        let mut buffer = [0u8; 512];
        // We read the first page to get the FileHeader so we use the 0 offset
        file.read_exact_at(&mut buffer, 0)?;

        let file_header = FileHeader::decode(&buffer).map_err(|e| PagerError::Corrupted {
            page: PageId::new(0),
            source: e,
        })?;

        let page_count = u64::from(file_header.page_count);
        let page_size = u64::from(file_header.page_size.get());
        let expected_file_size = page_count.saturating_mul(page_size);

        if file_len != expected_file_size {
            return Err(PagerError::FileSizeMismatch {
                expected: expected_file_size,
                actual: file_len,
            });
        }

        let mut pager = Self {
            file,
            file_header,
            scratch: vec![0u8; file_header.page_size.as_usize()].into_boxed_slice(),
            wal,
            sync_mode,
            recovery,
            op: None,
            wal_buf: Vec::new(),
            poisoned: false,
        };
        pager.file.read_exact_at(&mut pager.scratch, 0)?;

        verify_checksum(&pager.scratch).map_err(|e| PagerError::Corrupted {
            page: PageId::new(0),
            source: e,
        })?;

        Ok(pager)
    }

    /// What recovery did when this pager was opened (all zero after `create`).
    #[must_use]
    pub const fn recovery_report(&self) -> RecoveryReport {
        self.recovery
    }

    /// The write-ahead log of this database (read-only view).
    #[must_use]
    pub const fn wal(&self) -> &Wal {
        &self.wal
    }

    /// Returns the size of every page in the file.
    #[must_use]
    pub const fn page_size(&self) -> PageSize {
        self.file_header.page_size
    }

    /// Returns the number of pages in the file, including page 0. Valid page
    /// ids are `0..page_count`.
    #[must_use]
    pub const fn page_count(&self) -> u32 {
        self.file_header.page_count
    }

    /// Returns the root page of the catalog tree (`catalog_root` in the file
    /// header), or `None` if the database has no catalog yet.
    #[must_use]
    pub const fn catalog_root(&self) -> Option<PageId> {
        self.file_header.catalog_root
    }

    /// Sets the root page of the catalog tree in the file header, or clears
    /// it with `None`.
    ///
    /// Does not check the type of the page: the tree layer owns that. Does
    /// not sync. Runs as its own operation unless called inside
    /// [`Pager::atomic`].
    ///
    /// # Errors
    ///
    /// - [`PagerError::ReservedPage`] if `root` is page 0 (the file header).
    /// - [`PagerError::PageOutOfBounds`] if `root` is not below `page_count`.
    ///   In both cases nothing is written.
    /// - [`PagerError::Poisoned`] if an earlier commit failed.
    /// - [`PagerError::Wal`] or [`PagerError::Io`] if the commit fails; see
    ///   [`Pager::atomic`].
    pub fn set_catalog_root(&mut self, root: Option<PageId>) -> Result<(), PagerError> {
        self.atomic(|p| p.set_catalog_root_inner(root))
    }

    fn set_catalog_root_inner(&mut self, root: Option<PageId>) -> Result<(), PagerError> {
        if let Some(page) = root {
            if page.get() == 0 {
                return Err(PagerError::ReservedPage { page });
            }

            if page.get() >= self.page_count() {
                return Err(PagerError::PageOutOfBounds {
                    page,
                    page_count: self.page_count(),
                });
            }
        }

        let mut file_header = self.file_header;
        file_header.catalog_root = root;

        self.write_header(&file_header)?;
        self.file_header = file_header;

        Ok(())
    }

    /// Reads page `page_id` into `buffer` and verifies its checksum. Page 0
    /// can be read.
    ///
    /// # Errors
    ///
    /// - [`PagerError::PageOutOfBounds`] if `page_id` is not below
    ///   `page_count`.
    /// - [`PagerError::BufferSize`] if `buffer` is not exactly one page long.
    /// - [`PagerError::Io`] if the read fails.
    /// - [`PagerError::Corrupted`] with `page_id` if the checksum is wrong. A
    ///   page that was allocated but never written always fails here.
    /// - [`PagerError::Poisoned`] if an earlier commit failed.
    ///
    /// Inside an operation, a page changed earlier in the same operation is
    /// read from memory (read-your-writes).
    pub fn read_page(&self, page_id: PageId, buffer: &mut [u8]) -> Result<(), PagerError> {
        self.check_poisoned()?;
        if page_id.get() >= self.file_header.page_count {
            return Err(PagerError::PageOutOfBounds {
                page: page_id,
                page_count: self.file_header.page_count,
            });
        }

        let page_offset = page_id.offset(self.page_size());

        if buffer.len() != self.page_size().as_usize() {
            return Err(PagerError::BufferSize {
                expected: self.page_size().as_usize(),
                actual: buffer.len(),
            });
        }

        if let Some(op) = &self.op {
            if let Some(image) = op.dirty.get(&page_id) {
                buffer.copy_from_slice(image);
            } else if page_id.get() >= op.header_before.page_count {
                // Grown in this operation: the file has no bytes there yet.
                // Behave like the zero-filled page it will become.
                buffer.fill(0);
            } else {
                self.file.read_exact_at(buffer, page_offset)?;
            }
        } else {
            self.file.read_exact_at(buffer, page_offset)?;
        }

        verify_checksum(buffer).map_err(|e| PagerError::Corrupted {
            page: page_id,
            source: e,
        })?;

        Ok(())
    }

    /// Stamps the page LSN and the checksum into `buffer`, then writes it as
    /// page `page_id`.
    ///
    /// The page must already exist. Outside [`Pager::atomic`] the write is its
    /// own operation and is durable (in the WAL) when this returns; inside one
    /// it is held in memory until the operation commits.
    ///
    /// # Errors
    ///
    /// - [`PagerError::BufferSize`] if `buffer` is not exactly one page long.
    /// - [`PagerError::ReservedPage`] if `page_id` is 0 (the file header).
    /// - [`PagerError::PageOutOfBounds`] if `page_id` is not below
    ///   `page_count`. Call [`Pager::allocate_page`] first.
    /// - [`PagerError::Poisoned`] if an earlier commit failed.
    /// - [`PagerError::Wal`] or [`PagerError::Io`] if the commit fails; see
    ///   [`Pager::atomic`]. The LSN and checksum are already stamped into
    ///   `buffer` at that point.
    pub fn write_page(&mut self, page_id: PageId, buffer: &mut [u8]) -> Result<(), PagerError> {
        self.atomic(|p| p.write_page_inner(page_id, buffer))
    }

    fn write_page_inner(&mut self, page_id: PageId, buffer: &mut [u8]) -> Result<(), PagerError> {
        if buffer.len() != self.page_size().as_usize() {
            return Err(PagerError::BufferSize {
                expected: self.page_size().as_usize(),
                actual: buffer.len(),
            });
        }

        if page_id.get() == 0 {
            return Err(PagerError::ReservedPage { page: page_id });
        }

        if page_id.get() >= self.file_header.page_count {
            return Err(PagerError::PageOutOfBounds {
                page: page_id,
                page_count: self.file_header.page_count,
            });
        }

        let op = self.op.as_mut().ok_or(PagerError::NoActiveOperation)?;
        set_page_lsn(buffer, op.lsn)?;
        stamp_checksum(buffer)?;
        stage_image(op, page_id, buffer);

        Ok(())
    }

    /// Returns the id of a page the caller can write.
    ///
    /// If the free list is not empty, pops its head: reads the head page,
    /// takes its `next_free` link as the new head, and writes the file header
    /// with `freelist_count - 1`. The file does not grow. The returned page
    /// still holds the old free page until the caller writes it.
    ///
    /// If the free list is empty, raises `page_count` by one and writes the
    /// file header. The file itself grows when the operation commits, and the
    /// new page is filled with zeros, so reading it fails the checksum until
    /// its first [`Pager::write_page`].
    ///
    /// Does not sync. Runs as its own operation unless called inside
    /// [`Pager::atomic`].
    ///
    /// # Errors
    ///
    /// When popping the free list, the file and the pager are left unchanged
    /// on every error (the commit errors of [`Pager::atomic`] and
    /// [`PagerError::Poisoned`] also apply to every case):
    ///
    /// - [`PagerError::Corrupted`] with the head page if its checksum is
    ///   wrong.
    /// - [`PagerError::FreelistCorrupted`] with the head page if it is not a
    ///   free page, if its `next_free` is not below `page_count`, or if
    ///   `next_free` does not agree with the new `freelist_count` (set when
    ///   the count reaches 0, or missing when it does not).
    /// - [`PagerError::Io`] if reading the head or writing the file header
    ///   fails.
    ///
    /// When growing the file:
    ///
    /// - [`PagerError::DatabaseFull`] if `page_count` is already `u32::MAX`.
    pub fn allocate_page(&mut self) -> Result<PageId, PagerError> {
        self.atomic(Self::allocate_page_inner)
    }

    fn allocate_page_inner(&mut self) -> Result<PageId, PagerError> {
        if self.file_header.freelist_count != 0
            && let Some(head) = self.file_header.freelist_head
        {
            let mut buffer = vec![0u8; self.page_size().as_usize()];
            self.read_page(head, &mut buffer)?;

            let next_free_page_id =
                decode_free_page(&buffer).map_err(|_| PagerError::FreelistCorrupted {
                    page: head,
                    reason: "page is not a free_page",
                })?;
            let new_count = self.file_header.freelist_count.saturating_sub(1);
            if let Some(page_id) = next_free_page_id {
                if page_id.get() >= self.file_header.page_count {
                    return Err(PagerError::FreelistCorrupted {
                        page: head,
                        reason: "page_id is greater than page_count",
                    });
                }

                if new_count == 0 {
                    return Err(PagerError::FreelistCorrupted {
                        page: head,
                        reason: "free_list_count is 0 but head is set",
                    });
                }
            }

            if next_free_page_id.is_none() && new_count > 0 {
                return Err(PagerError::FreelistCorrupted {
                    page: head,
                    reason: "page_id is none and new_count is greater than 0",
                });
            }

            let mut file_header = self.file_header;
            file_header.freelist_head = next_free_page_id;
            file_header.freelist_count = new_count;

            self.write_header(&file_header)?;

            self.file_header = file_header;

            return Ok(head);
        }

        let page_id = PageId::new(self.file_header.page_count);
        let mut file_header = self.file_header;
        file_header.page_count = self
            .file_header
            .page_count
            .checked_add(1)
            .ok_or(PagerError::DatabaseFull)?;

        self.write_header(&file_header)?;

        self.file_header = file_header;

        Ok(page_id)
    }

    /// Walks the free list from the head and checks that it matches the file
    /// header: exactly `freelist_count` pages, each one below `page_count`
    /// and a valid free page, with no page twice. Changes nothing.
    ///
    /// Follows at most `freelist_count` links, so it never loops forever: a
    /// cycle (for example from freeing the same page twice) never reaches the
    /// end of the list and is reported as a list longer than the count.
    ///
    /// # Errors
    ///
    /// - [`PagerError::Corrupted`] with the page whose checksum is wrong.
    /// - [`PagerError::FreelistCorrupted`] if a page is not a free page
    ///   (`page` is that page), a link points past the end of the file
    ///   (`page` is the page that holds the link, or 0 for the file header),
    ///   or the list is shorter or longer than `freelist_count`.
    /// - [`PagerError::Io`] if a read fails.
    pub fn verify_freelist(&self) -> Result<(), PagerError> {
        self.check_poisoned()?;
        let mut current = self.file_header.freelist_head;
        let mut holder = None;
        let mut buffer = vec![0u8; self.page_size().as_usize()];

        for _ in 0..self.free_page_count() {
            if let Some(head) = current {
                if head.get() >= self.page_count() {
                    return Err(PagerError::FreelistCorrupted {
                        page: holder.unwrap_or(PageId::HEADER),
                        reason: "head out of page range",
                    });
                }

                self.read_page(head, &mut buffer)?;

                let next =
                    decode_free_page(&buffer).map_err(|_| PagerError::FreelistCorrupted {
                        page: head,
                        reason: "error decoding free list",
                    })?;

                holder = current;
                current = next;
            } else {
                return Err(PagerError::FreelistCorrupted {
                    page: holder.unwrap_or(PageId::HEADER),
                    reason: "list is smaller than freelist_count",
                });
            }
        }

        if let Some(curr) = current {
            return Err(PagerError::FreelistCorrupted {
                page: curr,
                reason: "list larger than count",
            });
        }

        Ok(())
    }

    /// Flushes every write to the file down to the disk (`fsync`), so it
    /// survives a crash or power loss.
    ///
    /// # Errors
    ///
    /// - [`PagerError::Io`] if the operating system cannot flush the file to
    ///   disk.
    pub fn sync(&self) -> Result<(), PagerError> {
        self.check_poisoned()?;
        if self.sync_mode == SyncMode::Full {
            self.file.sync_all()?;
        }

        Ok(())
    }

    /// Pushes `page` onto the free list so a later [`Pager::allocate_page`]
    /// can reuse it.
    ///
    /// Writes the page first, as a free page linked to the current head, and
    /// then the file header with `page` as the new head and
    /// `freelist_count + 1` (ADR 0003). Does not read the page, so it cannot
    /// detect a double free: freeing a page twice links it to itself, and
    /// [`Pager::verify_freelist`] reports the cycle. Does not sync.
    ///
    /// Both writes belong to one operation, so a crash leaves both or neither.
    /// Runs as its own operation unless called inside [`Pager::atomic`].
    ///
    /// # Errors
    ///
    /// - [`PagerError::ReservedPage`] if `page` is 0 (the file header).
    /// - [`PagerError::PageOutOfBounds`] if `page` is not below `page_count`.
    ///   In both cases nothing is written.
    /// - [`PagerError::Poisoned`] if an earlier commit failed.
    /// - [`PagerError::Wal`] or [`PagerError::Io`] if the commit fails; see
    ///   [`Pager::atomic`].
    pub fn free_page(&mut self, page: PageId) -> Result<(), PagerError> {
        self.atomic(|p| p.free_page_inner(page))
    }

    fn free_page_inner(&mut self, page: PageId) -> Result<(), PagerError> {
        let page_id = page.get();
        if page_id == 0 {
            return Err(PagerError::ReservedPage { page });
        }

        if page_id >= self.page_count() {
            return Err(PagerError::PageOutOfBounds {
                page,
                page_count: self.page_count(),
            });
        }

        let mut file_header = self.file_header;
        let mut buffer = vec![0u8; self.page_size().as_usize()];

        encode_free_page(&mut buffer, self.file_header.freelist_head)?;
        self.write_page_inner(page, &mut buffer)?;

        let free_list_cont = self.file_header.freelist_count.saturating_add(1);
        file_header.freelist_count = free_list_cont;
        file_header.freelist_head = Some(page);

        self.write_header(&file_header)?;
        self.file_header = file_header;

        Ok(())
    }

    /// Returns the number of pages in the free list (`freelist_count` in the
    /// file header).
    #[must_use]
    pub const fn free_page_count(&self) -> u32 {
        self.file_header.freelist_count
    }

    /// Encodes `header` into the scratch buffer, stamps its LSN and checksum,
    /// and stages it as page 0 of the current operation. Does not update
    /// `self.file_header`: the caller does that once this succeeds.
    fn write_header(&mut self, header: &FileHeader) -> Result<(), PagerError> {
        let op = self.op.as_mut().ok_or(PagerError::NoActiveOperation)?;
        header.encode(&mut self.scratch)?;
        set_page_lsn(&mut self.scratch, op.lsn)?;
        stamp_checksum(&mut self.scratch)?;
        stage_image(op, PageId::HEADER, &self.scratch);

        Ok(())
    }

    /// Runs `f` as one atomic operation (ADR 0006, D4): all of its writes
    /// reach the database, or none do.
    ///
    /// Outside an operation this begins one, runs `f`, and commits if `f`
    /// returns `Ok` or aborts if it returns `Err`. Abort drops every change
    /// and restores the in-memory file header, `page_count` included; nothing
    /// reached either file. Inside an operation `f` simply runs as part of the
    /// outer one: an `Err` from a nested call only aborts if the caller
    /// propagates it out of the outer closure.
    ///
    /// Commit (D5) encodes one WAL record with every changed page, appends
    /// and fsyncs it, writes the pages to the database file and sets its
    /// length. An operation that changed nothing writes no record. This is
    /// the "no-steal, no-force" policy of _Database Internals_, Ch. 5.
    ///
    /// # Errors
    ///
    /// Whatever `f` returns, or a [`PagerError`] converted into `E`:
    ///
    /// - [`PagerError::Poisoned`] if an earlier commit failed.
    /// - [`PagerError::Wal`] if the record cannot be encoded (the operation is
    ///   discarded and the pager stays usable) or cannot be appended or
    ///   fsynced (the pager is poisoned).
    /// - [`PagerError::Io`] if applying the pages to the database file fails
    ///   after the fsync (the pager is poisoned; reopen to recover).
    pub fn atomic<T, E: From<PagerError>>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E> {
        self.check_poisoned()?;
        if self.op.is_some() {
            return f(self);
        }

        self.op = Some(OpState {
            dirty: BTreeMap::new(),
            header_before: self.file_header,
            lsn: self.wal.next_lsn(),
        });

        match f(self) {
            Ok(value) => {
                self.commit()?;
                Ok(value)
            }
            Err(e) => {
                self.abort();
                Err(e)
            }
        }
    }

    const fn check_poisoned(&self) -> Result<(), PagerError> {
        if self.poisoned {
            Err(PagerError::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Drops the current operation and restores the in-memory header.
    fn abort(&mut self) {
        if let Some(op) = self.op.take() {
            self.file_header = op.header_before;
        }
    }

    /// Makes the current operation durable (ADR 0006, D5).
    fn commit(&mut self) -> Result<(), PagerError> {
        let Some(op) = self.op.take() else {
            return Ok(());
        };
        if op.dirty.is_empty() {
            return Ok(());
        }

        let encoded = encode_record(
            &mut self.wal_buf,
            op.lsn,
            self.file_header.page_size,
            op.dirty.iter().map(|(id, image)| (*id, image.as_slice())),
        );
        if let Err(e) = encoded {
            self.file_header = op.header_before;
            return Err(e.into());
        }

        // The append writes and fsyncs in one call, so a failure here may be
        // either; it poisons to be safe (a failed fsync cannot be retried).
        if let Err(e) = self.wal.append(&self.wal_buf, self.sync_mode) {
            self.poisoned = true;
            return Err(e.into());
        }

        if let Err(e) = self.apply(&op) {
            self.poisoned = true;
            return Err(e);
        }

        Ok(())
    }

    /// Writes every image of a committed operation to the database file, then
    /// sets the file length to `page_count * page_size`.
    fn apply(&self, op: &OpState) -> Result<(), PagerError> {
        let page_size = self.page_size();
        for (id, image) in &op.dirty {
            self.file.write_all_at(image, id.offset(page_size))?;
        }
        let len = u64::from(self.file_header.page_count)
            .checked_mul(u64::from(page_size.get()))
            .ok_or(PagerError::DatabaseFull)?;
        self.file.set_len(len)?;

        Ok(())
    }
}

/// Copies `image` into the operation's dirty map, reusing the entry's buffer.
fn stage_image(op: &mut OpState, id: PageId, image: &[u8]) {
    if let Some(slot) = op.dirty.get_mut(&id) {
        slot.clear();
        slot.extend_from_slice(image);
    } else {
        op.dirty.insert(id, image.to_vec());
    }
}

/// Opens the WAL next to `path`. A missing WAL (a Phase 2 file, or a crash
/// between creating the database and the WAL) is created with `base_lsn = 0`,
/// taking the page size from the database header.
fn open_or_create_wal(file: &File, path: &Path, sync: SyncMode) -> Result<Wal, PagerError> {
    let wal_file = wal_path(path);
    match Wal::open(&wal_file) {
        Err(WalError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            let len = file.metadata()?.len();
            if len < u64::from(PageSize::MIN) {
                return Err(PagerError::FileTooSmall { len });
            }
            let mut prefix = [0u8; 512];
            file.read_exact_at(&mut prefix, 0)?;
            let header = FileHeader::decode(&prefix).map_err(|source| PagerError::Corrupted {
                page: PageId::new(0),
                source,
            })?;
            let wal = Wal::create(&wal_file, header.page_size, sync)?;
            sync_parent_dir(path)?;
            Ok(wal)
        }
        other => Ok(other?),
    }
}

/// fsyncs the directory that holds `path` (`.` when the path has no parent).
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};

    /// Poisoning cannot be triggered from outside (it needs a failing fsync),
    /// so the test sets the flag and checks that every public entry point
    /// refuses to run.
    #[test]
    fn poisoned_pager_rejects_every_call() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut pager = Pager::create(&dir.path().join("p.db"), PagerOptions::default())?;
        pager.poisoned = true;

        let mut buf = vec![0u8; pager.page_size().as_usize()];
        let id = PageId::new(1);
        ensure!(matches!(
            pager.read_page(PageId::HEADER, &mut buf),
            Err(PagerError::Poisoned)
        ));
        ensure!(matches!(
            pager.write_page(id, &mut buf),
            Err(PagerError::Poisoned)
        ));
        ensure!(matches!(pager.allocate_page(), Err(PagerError::Poisoned)));
        ensure!(matches!(pager.free_page(id), Err(PagerError::Poisoned)));
        ensure!(matches!(
            pager.set_catalog_root(None),
            Err(PagerError::Poisoned)
        ));
        ensure!(matches!(pager.sync(), Err(PagerError::Poisoned)));
        ensure!(matches!(pager.verify_freelist(), Err(PagerError::Poisoned)));
        Ok(())
    }
}
