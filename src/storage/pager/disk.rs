//! Disk I/O: reading and writing pages at `page_id * page_size` offsets.

use std::{
    fs::{File, OpenOptions},
    os::unix::fs::FileExt,
    path::Path,
};

use crate::storage::pager::{
    error::PagerError,
    file_header::FileHeader,
    freelist::{decode_free_page, encode_free_page},
    page::{PageType, page_type, stamp_checksum, verify_checksum},
    types::{PageId, PageSize},
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
#[derive(Debug)]
pub struct Pager {
    /// The database file, open for reading and writing.
    file: File,
    /// In-memory copy of the file header. Always matches page 0 on disk.
    file_header: FileHeader,
    /// One page of reusable memory for writing page 0, so updating the header
    /// does not allocate.
    scratch: Box<[u8]>,
}

impl Pager {
    /// Creates a new database file at `path` that holds only page 0 (the file
    /// header), then syncs it to disk.
    ///
    /// # Errors
    ///
    /// - [`PagerError::Io`] if the file already exists
    ///   ([`std::io::ErrorKind::AlreadyExists`]), or if creating it, writing
    ///   page 0, or syncing fails.
    pub fn create(path: &Path, pager_option: PagerOptions) -> Result<Self, PagerError> {
        let file = File::create_new(path)?;

        let file_header = FileHeader::new(pager_option.page_size);

        let mut pager = Self {
            file,
            file_header,
            scratch: vec![0u8; pager_option.page_size.as_usize()].into_boxed_slice(),
        };

        pager.write_header(&file_header)?;
        pager.sync()?;

        Ok(pager)
    }

    /// Opens an existing database file at `path` and validates it: the file
    /// header, the file length, and the checksum of page 0. The page size
    /// comes from the file header, not from the caller.
    ///
    /// # Errors
    ///
    /// The checks run in this order, and the first one that fails is returned:
    ///
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
        };
        pager.file.read_exact_at(&mut pager.scratch, 0)?;

        verify_checksum(&pager.scratch).map_err(|e| PagerError::Corrupted {
            page: PageId::new(0),
            source: e,
        })?;

        Ok(pager)
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
    pub fn read_page(&self, page_id: PageId, buffer: &mut [u8]) -> Result<(), PagerError> {
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

        self.file.read_exact_at(buffer, page_offset)?;

        verify_checksum(buffer).map_err(|e| PagerError::Corrupted {
            page: page_id,
            source: e,
        })?;

        Ok(())
    }

    /// Stamps the checksum into `buffer`, then writes it as page `page_id`.
    ///
    /// The page must already exist. The write is not durable until
    /// [`Pager::sync`].
    ///
    /// # Errors
    ///
    /// - [`PagerError::BufferSize`] if `buffer` is not exactly one page long.
    /// - [`PagerError::ReservedPage`] if `page_id` is 0 (the file header).
    /// - [`PagerError::PageOutOfBounds`] if `page_id` is not below
    ///   `page_count`. Call [`Pager::allocate_page`] first.
    /// - [`PagerError::Io`] if the write fails. The checksum is already
    ///   stamped into `buffer` at that point.
    pub fn write_page(&mut self, page_id: PageId, buffer: &mut [u8]) -> Result<(), PagerError> {
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

        stamp_checksum(buffer)?;

        self.file
            .write_all_at(buffer, page_id.offset(self.page_size()))?;

        Ok(())
    }

    /// Grows the file by one page, writes the new `page_count` to the file
    /// header, and returns the id of the new page.
    ///
    /// The new page is filled with zeros, so reading it fails the checksum
    /// until its first [`Pager::write_page`]. Does not sync.
    ///
    /// # Errors
    ///
    /// - [`PagerError::DatabaseFull`] if `page_count` is already `u32::MAX`.
    /// - [`PagerError::Io`] if growing the file or writing the file header
    ///   fails. The in-memory `page_count` does not change, but the file may
    ///   already be one page longer, so a later [`Pager::open`] can fail with
    ///   [`PagerError::FileSizeMismatch`].
    pub fn allocate_page(&mut self) -> Result<PageId, PagerError> {
        if self.file_header.freelist_count != 0
            && let Some(head) = self.file_header.freelist_head
        {
            let mut buffer = vec![0u8; self.page_size().as_usize()];
            self.read_page(head, &mut buffer)?;
            let page_type = page_type(&buffer)?;
            if page_type != PageType::Free {
                return Err(PagerError::FreeListCorrupted {
                    page: head,
                    reason: "head is not a free page",
                });
            }

            let next_free_page_id = decode_free_page(&buffer)?;
            self.file_header.freelist_head = next_free_page_id;
            let new_count = self.file_header.freelist_count.saturating_sub(1);
            if let Some(page_id) = next_free_page_id {
                if page_id.get() >= self.file_header.page_count {
                    return Err(PagerError::FreeListCorrupted {
                        page: page_id,
                        reason: "page_id is greater than page_count",
                    });
                }

                if new_count == 0 {
                    return Err(PagerError::FreeListCorrupted {
                        page: page_id,
                        reason: "page_count is 0 but head is set",
                    });
                }
            }

            if next_free_page_id.is_none() && new_count > 0 {
                return Err(PagerError::FreeListCorrupted {
                    page: PageId::new(0),
                    reason: "page_id is none and new_count is greater than 0",
                });
            }

            encode_free_page(&mut buffer, Some(head))?;

            return Ok(head);
        }

        let page_id = PageId::new(self.file_header.page_count);

        let mut file_header = self.file_header;
        file_header.page_count = self
            .file_header
            .page_count
            .checked_add(1)
            .ok_or(PagerError::DatabaseFull)?;

        let new_len = u64::from(file_header.page_count)
            .checked_mul(u64::from(self.file_header.page_size.get()))
            .ok_or(PagerError::DatabaseFull)?;

        self.file.set_len(new_len)?;

        self.write_header(&file_header)?;

        self.file_header = file_header;

        Ok(page_id)
    }

    pub fn verify_freelist(&self) -> Result<(), PagerError> {
        let mut current = self.file_header.freelist_head;
        let mut holder = None;
        let buffer = vec![0u8; self.page_size().as_usize()];

        for _ in 0..self.free_page_count() {
            if current.is_none() {
                return Err(PagerError::FreeListCorrupted {
                    page: PageId::new(0),
                    reason: "list is smaller than list_count",
                });
            }

            if let Some(head) = current {
                if head.get() >= self.free_page_count() {
                    return Err(PagerError::FreeListCorrupted {
                        page: holder.unwrap_or(PageId::new(0)),
                        reason: "",
                    });
                }

                let next =
                    decode_free_page(&buffer).map_err(|_| PagerError::FreeListCorrupted {
                        page: head,
                        reason: "error decoding free list",
                    })?;

                holder = current;
                current = next;
            }
        }

        if let Some(curr) = current {
            return Err(PagerError::FreeListCorrupted {
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
        self.file.sync_all()?;

        Ok(())
    }

    pub fn free_page(&mut self, page: PageId) -> Result<(), PagerError> {
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

        let page_size = self.page_size();
        self.file
            .read_exact_at(&mut self.scratch, page.offset(page_size))?;
        encode_free_page(&mut self.scratch, self.file_header.freelist_head)?;
        let free_list_cont = self.file_header.freelist_count.saturating_add(1);
        self.file_header.freelist_count = free_list_cont;

        Ok(())
    }

    pub const fn free_page_count(&self) -> u32 {
        self.file_header.freelist_count
    }

    /// Encodes `header` into the scratch buffer, stamps its checksum, and
    /// writes it as page 0. Does not update `self.file_header`: the caller
    /// does that once the write succeeds.
    fn write_header(&mut self, header: &FileHeader) -> Result<(), PagerError> {
        header.encode(&mut self.scratch)?;

        stamp_checksum(&mut self.scratch)?;
        self.file.write_all_at(&self.scratch, 0)?;

        Ok(())
    }
}
