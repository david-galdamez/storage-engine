//! Step 6 (TDD): file-backed pager with page I/O (ADR 0003).
//!
//! The pager owns one database file. Page N lives at byte offset
//! `N * page_size`. Page 0 holds the file header (step 5). Every page carries
//! the common header (step 4): the pager stamps the checksum on every write and
//! verifies it on every read, so layouts never deal with it.
//!
//! API under test (module `storage::pager::disk`):
//!
//! - `PagerOptions { pub page_size: PageSize }`, with `Default` (4096 bytes).
//! - `Pager`:
//!   - `create(&Path, PagerOptions) -> Result<Pager, PagerError>`: creates a
//!     new file (fails if it exists) with only page 0, then syncs it.
//!   - `open(&Path) -> Result<Pager, PagerError>`: opens an existing file and
//!     validates it, in this order:
//!     1. file shorter than 512 bytes: `FileTooSmall`
//!     2. read a 512-byte prefix and decode the file header; a decode error
//!        becomes `Corrupted { page: 0, source }`
//!     3. file length must be exactly `page_count * page_size`:
//!        `FileSizeMismatch`
//!     4. read the whole page 0 and verify its checksum:
//!        `Corrupted { page: 0, source: ChecksumMismatch }`
//!   - `page_size(&self) -> PageSize` and `page_count(&self) -> u32`.
//!   - `read_page(&self, PageId, &mut [u8]) -> Result<(), PagerError>`: fills
//!     the buffer and verifies the checksum. Takes `&self`: reading never
//!     changes the pager. Page 0 can be read.
//!   - `write_page(&mut self, PageId, &mut [u8]) -> Result<(), PagerError>`:
//!     stamps the checksum into the buffer, then writes it. Page 0 is reserved
//!     for the pager and cannot be written.
//!   - `allocate_page(&mut self) -> Result<PageId, PagerError>`: grows the file
//!     by one page and returns its id. Updates `page_count` in the file header
//!     on disk. The new page is undefined until its first write, so reading it
//!     fails the checksum. (The free list comes in step 7.)
//!   - `sync(&self) -> Result<(), PagerError>`: flushes the file to disk.
//!
//! New `PagerError` variants:
//!
//! - `BufferSize { expected: usize, actual: usize }`: the buffer passed to
//!   `read_page` or `write_page` is not exactly one page.
//! - `PageOutOfBounds { page: PageId, page_count: u32 }`: the page does not
//!   exist yet.
//! - `ReservedPage { page: PageId }`: `write_page` on page 0.
//! - `FileTooSmall { len: u64 }`: the file cannot even hold the header prefix.
//! - `FileSizeMismatch { expected: u64, actual: u64 }`: in bytes.
//! - `DatabaseFull`: `allocate_page` when `page_count` is already `u32::MAX`.
//!   (Not tested: it needs a file of 2 TiB or more.)
//!
//! Errors from the operating system stay in `PagerError::Io`. A page that
//! fails validation is reported as `Corrupted { page, source }`, with the
//! page id and the `PageError` behind it.

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use proptest::prelude::*;
use storage_engine_rs::storage::pager::codec::{write_bytes, write_u16, write_u32};
use storage_engine_rs::storage::pager::crc32::crc32;
use storage_engine_rs::storage::pager::disk::{Pager, PagerOptions};
use storage_engine_rs::storage::pager::error::{PageError, PagerError};
use storage_engine_rs::storage::pager::file_header::FileHeader;
use storage_engine_rs::storage::pager::page::{
    PageType, set_page_lsn, set_page_type, verify_checksum,
};
use storage_engine_rs::storage::pager::types::{PageId, PageSize};
use tempfile::TempDir;

/// Page sizes allowed by ADR 0001 that the tests cover: smallest, default, largest.
const SIZES: [u32; 3] = [512, 4096, 65536];

/// CRC32 of 508 zero bytes: what a never-written 512-byte page computes to.
/// Computed with Python's `zlib.crc32(bytes(508))`.
const ZERO_PAGE_512_CRC: u32 = 0x0FBB_6D54;

// File header offsets inside page 0, written here independently of the code.
const OFF_MAGIC: u64 = 16;
const OFF_VERSION: u64 = 24;
const OFF_RESERVED: u64 = 26;
const OFF_PAGE_SIZE: u64 = 28;
const OFF_PAGE_COUNT: u64 = 32;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A temporary directory and the path of a database file inside it. The file
/// does not exist yet. Keep the `TempDir` alive: dropping it deletes the
/// directory.
fn temp_db() -> anyhow::Result<(TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("test.db");
    Ok((dir, path))
}

/// Creates a new database with pages of `size` bytes.
fn create(path: &Path, size: u32) -> anyhow::Result<Pager> {
    Ok(Pager::create(
        path,
        PagerOptions {
            page_size: PageSize::new(size)?,
        },
    )?)
}

/// Creates a database with `size`-byte pages and allocates `extra` pages.
fn create_with_pages(path: &Path, size: u32, extra: u32) -> anyhow::Result<Pager> {
    let mut pager = create(path, size)?;
    for _ in 0..extra {
        pager.allocate_page()?;
    }
    Ok(pager)
}

/// A page buffer of `size` bytes: type Slotted and a body derived from `seed`,
/// so that different seeds give different pages.
fn page_with(size: usize, seed: u8) -> anyhow::Result<Vec<u8>> {
    let mut buf: Vec<u8> = (0..size)
        .map(|i| {
            let [low, ..] = i.to_le_bytes();
            low ^ seed
        })
        .collect();
    set_page_type(&mut buf, PageType::Slotted)?;
    set_page_lsn(&mut buf, 0)?;
    Ok(buf)
}

/// Length of the file at `path` in bytes.
fn file_len(path: &Path) -> anyhow::Result<u64> {
    Ok(fs::metadata(path)?.len())
}

/// Overwrites bytes of the file at `path`, starting at `offset`, without the
/// pager. Used to simulate corruption.
fn patch(path: &Path, offset: u64, bytes: &[u8]) -> anyhow::Result<()> {
    use std::os::unix::fs::FileExt;
    let file = OpenOptions::new().write(true).open(path)?;
    file.write_all_at(bytes, offset)?;
    Ok(())
}

/// XORs one byte of the file at `path` with `0xFF`.
fn flip_byte(path: &Path, offset: u64) -> anyhow::Result<()> {
    let data = fs::read(path)?;
    let index = usize::try_from(offset)?;
    let byte = data.get(index).context("offset past end of file")?;
    patch(path, offset, &[byte ^ 0xFF])
}

/// Sets the file at `path` to `len` bytes, cutting or zero-extending it.
fn set_file_len(path: &Path, len: u64) -> anyhow::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(len)?;
    Ok(())
}

/// Reads page `id` straight from the file, without the pager.
fn raw_page(path: &Path, size: usize, id: u32) -> anyhow::Result<Vec<u8>> {
    let data = fs::read(path)?;
    let start = size
        .checked_mul(usize::try_from(id)?)
        .context("offset overflow")?;
    let end = start.checked_add(size).context("offset overflow")?;
    Ok(data
        .get(start..end)
        .context("page past end of file")?
        .to_vec())
}

/// Checks that the stored checksum of a raw page is the CRC32 of `[4, len)`.
fn raw_checksum_ok(page: &[u8]) -> anyhow::Result<bool> {
    let stored = u32::from_le_bytes(page.get(0..4).context("checksum")?.try_into()?);
    Ok(stored == crc32(page.get(4..).context("body")?))
}

/// Opens `path` and returns the error, failing the test if `open` succeeds.
fn open_err(path: &Path) -> anyhow::Result<PagerError> {
    Pager::open(path)
        .err()
        .context("open succeeded but an error was expected")
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

#[test]
fn default_options_use_4_kib_pages() {
    assert_eq!(PagerOptions::default().page_size.get(), 4096);
}

#[test]
fn create_writes_only_page_0() -> anyhow::Result<()> {
    for size in SIZES {
        let (_dir, path) = temp_db()?;
        let pager = create(&path, size)?;
        ensure!(pager.page_count() == 1, "size {size}: page_count");
        ensure!(pager.page_size().get() == size, "size {size}: page_size");
        ensure!(
            file_len(&path)? == u64::from(size),
            "size {size}: file must hold exactly one page"
        );
    }
    Ok(())
}

#[test]
fn create_writes_a_valid_file_header() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    create(&path, 512)?;
    let page0 = raw_page(&path, 512, 0)?;
    ensure!(raw_checksum_ok(&page0)?, "page 0 checksum is not stamped");
    let header = FileHeader::decode(&page0)?;
    ensure!(
        header == FileHeader::new(PageSize::new(512)?),
        "got {header:?}"
    );
    Ok(())
}

#[test]
fn create_fails_if_the_file_exists() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    fs::write(&path, b"precious data")?;
    let result = Pager::create(&path, PagerOptions::default()).err();
    ensure!(
        matches!(&result, Some(PagerError::Io(e)) if e.kind() == ErrorKind::AlreadyExists),
        "got {result:?}"
    );
    ensure!(
        fs::read(&path)? == b"precious data",
        "create must never overwrite an existing file"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// open
// ---------------------------------------------------------------------------

#[test]
fn open_after_create_for_every_size() -> anyhow::Result<()> {
    for size in SIZES {
        let (_dir, path) = temp_db()?;
        drop(create(&path, size)?);
        let pager = Pager::open(&path)?;
        ensure!(pager.page_size().get() == size, "size {size}: page_size");
        ensure!(pager.page_count() == 1, "size {size}: page_count");
    }
    Ok(())
}

#[test]
fn open_missing_file_is_not_found() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let err = open_err(&path)?;
    ensure!(
        matches!(&err, PagerError::Io(e) if e.kind() == ErrorKind::NotFound),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn open_rejects_files_too_small_for_the_header() -> anyhow::Result<()> {
    for len in [0, 1, 100, 511] {
        let (_dir, path) = temp_db()?;
        fs::write(&path, vec![0u8; len])?;
        let err = open_err(&path)?;
        let expected = u64::try_from(len)?;
        ensure!(
            matches!(err, PagerError::FileTooSmall { len: l } if l == expected),
            "len {len}: got {err:?}"
        );
    }
    Ok(())
}

#[test]
fn open_rejects_bad_magic() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    drop(create(&path, 512)?);
    patch(&path, OFF_MAGIC, b"NOTADB\0\0")?;
    let err = open_err(&path)?;
    ensure!(
        matches!(
            err,
            PagerError::Corrupted {
                page,
                source: PageError::BadMagic { .. }
            } if page == PageId::HEADER
        ),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn open_rejects_a_file_that_is_not_a_database() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    fs::write(&path, vec![0x42u8; 4096])?;
    let err = open_err(&path)?;
    ensure!(
        matches!(
            err,
            PagerError::Corrupted {
                source: PageError::BadMagic { .. },
                ..
            }
        ),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn open_rejects_unsupported_version() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    drop(create(&path, 512)?);
    let mut bytes = [0u8; 2];
    write_u16(&mut bytes, 0, 2)?;
    patch(&path, OFF_VERSION, &bytes)?;
    let err = open_err(&path)?;
    ensure!(
        matches!(
            err,
            PagerError::Corrupted {
                source: PageError::UnsupportedVersion { version: 2 },
                ..
            }
        ),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn open_rejects_invalid_page_size() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    drop(create(&path, 512)?);
    let mut bytes = [0u8; 4];
    write_u32(&mut bytes, 0, 1000)?;
    patch(&path, OFF_PAGE_SIZE, &bytes)?;
    let err = open_err(&path)?;
    ensure!(
        matches!(
            err,
            PagerError::Corrupted {
                source: PageError::InvalidPageSize { size: 1000 },
                ..
            }
        ),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn open_rejects_truncated_file() -> anyhow::Result<()> {
    // Three pages of 512 bytes, then the last byte is lost.
    let (_dir, path) = temp_db()?;
    drop(create_with_pages(&path, 512, 2)?);
    set_file_len(&path, 1535)?;
    let err = open_err(&path)?;
    ensure!(
        matches!(
            err,
            PagerError::FileSizeMismatch {
                expected: 1536,
                actual: 1535
            }
        ),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn open_rejects_file_with_extra_bytes() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    drop(create_with_pages(&path, 512, 2)?);
    set_file_len(&path, 1537)?;
    let err = open_err(&path)?;
    ensure!(
        matches!(
            err,
            PagerError::FileSizeMismatch {
                expected: 1536,
                actual: 1537
            }
        ),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn open_checks_file_size_before_page_0_checksum() -> anyhow::Result<()> {
    // Changing page_count breaks the checksum of page 0 too, but the size
    // check comes first and gives the more precise error.
    let (_dir, path) = temp_db()?;
    drop(create(&path, 512)?);
    let mut bytes = [0u8; 4];
    write_u32(&mut bytes, 0, 5)?;
    patch(&path, OFF_PAGE_COUNT, &bytes)?;
    let err = open_err(&path)?;
    ensure!(
        matches!(
            err,
            PagerError::FileSizeMismatch {
                expected: 2560,
                actual: 512
            }
        ),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn open_rejects_corrupted_page_0() -> anyhow::Result<()> {
    // Byte 100 is past the header fields: decode accepts it, the checksum does not.
    let (_dir, path) = temp_db()?;
    drop(create(&path, 512)?);
    flip_byte(&path, 100)?;
    let err = open_err(&path)?;
    ensure!(
        matches!(
            err,
            PagerError::Corrupted {
                page,
                source: PageError::ChecksumMismatch { .. }
            } if page == PageId::HEADER
        ),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn open_rejects_changed_reserved_byte_in_page_0() -> anyhow::Result<()> {
    // decode ignores reserved bytes, so only the checksum can catch this.
    let (_dir, path) = temp_db()?;
    drop(create(&path, 512)?);
    patch(&path, OFF_RESERVED, &[0x01])?;
    let err = open_err(&path)?;
    ensure!(
        matches!(
            err,
            PagerError::Corrupted {
                source: PageError::ChecksumMismatch { .. },
                ..
            }
        ),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn open_verifies_the_whole_page_0_for_large_pages() -> anyhow::Result<()> {
    // open first reads only 512 bytes. A change past them must still be caught.
    let (_dir, path) = temp_db()?;
    drop(create(&path, 65536)?);
    flip_byte(&path, 65535)?;
    let err = open_err(&path)?;
    ensure!(
        matches!(
            err,
            PagerError::Corrupted {
                source: PageError::ChecksumMismatch { .. },
                ..
            }
        ),
        "got {err:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// allocate_page
// ---------------------------------------------------------------------------

#[test]
fn allocate_returns_consecutive_ids_and_grows_the_file() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create(&path, 512)?;
    for expected in 1..=3u32 {
        let id = pager.allocate_page()?;
        ensure!(
            id == PageId::new(expected),
            "got page {id}, expected {expected}"
        );
        let total = expected.checked_add(1).context("overflow")?;
        ensure!(pager.page_count() == total, "page_count after {expected}");
        ensure!(
            file_len(&path)? == u64::from(total).checked_mul(512).context("overflow")?,
            "file length after allocating page {expected}"
        );
    }
    Ok(())
}

#[test]
fn allocate_updates_the_header_on_disk() -> anyhow::Result<()> {
    // No sync and no reopen: the header must already be written to the file.
    let (_dir, path) = temp_db()?;
    let mut pager = create(&path, 512)?;
    pager.allocate_page()?;
    pager.allocate_page()?;
    let header_page = raw_page(&path, 512, 0)?;
    ensure!(
        raw_checksum_ok(&header_page)?,
        "page 0 checksum not restamped"
    );
    let header = FileHeader::decode(&header_page)?;
    ensure!(
        header.page_count == 3,
        "page_count on disk: {}",
        header.page_count
    );
    Ok(())
}

#[test]
fn allocated_pages_persist_after_reopen() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    drop(create_with_pages(&path, 4096, 3)?);
    let mut pager = Pager::open(&path)?;
    ensure!(pager.page_count() == 4, "page_count after reopen");
    let next = pager.allocate_page()?;
    ensure!(next == PageId::new(4), "next id after reopen is {next}");
    Ok(())
}

#[test]
fn reading_a_page_before_its_first_write_fails_the_checksum() -> anyhow::Result<()> {
    // A new page is all zeros, and the CRC of zeros is not zero.
    let (_dir, path) = temp_db()?;
    let mut pager = create(&path, 512)?;
    let id = pager.allocate_page()?;
    let mut buf = vec![0u8; 512];
    let err = pager
        .read_page(id, &mut buf)
        .err()
        .context("read of an unwritten page succeeded")?;
    ensure!(
        matches!(
            err,
            PagerError::Corrupted {
                page,
                source: PageError::ChecksumMismatch {
                    stored: 0,
                    computed: ZERO_PAGE_512_CRC
                }
            } if page == id
        ),
        "got {err:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// write_page and read_page
// ---------------------------------------------------------------------------

#[test]
fn write_then_read_round_trips_for_every_size() -> anyhow::Result<()> {
    for size in SIZES {
        let (_dir, path) = temp_db()?;
        let len = usize::try_from(size)?;
        let mut pager = create_with_pages(&path, size, 1)?;
        let id = PageId::new(1);

        let mut written = page_with(len, 0x5A)?;
        pager.write_page(id, &mut written)?;
        let mut read = vec![0u8; len];
        pager.read_page(id, &mut read)?;
        ensure!(read == written, "size {size}: page changed on the way");
    }
    Ok(())
}

#[test]
fn write_stamps_the_checksum_into_the_buffer() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 512, 1)?;
    let mut buf = page_with(512, 1)?;
    write_bytes(&mut buf, 0, &[0xFF; 4])?; // stale checksum
    pager.write_page(PageId::new(1), &mut buf)?;
    verify_checksum(&buf)?;
    ensure!(
        raw_page(&path, 512, 1)? == buf,
        "the file must hold exactly the stamped buffer"
    );
    Ok(())
}

#[test]
fn pages_do_not_overlap() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 512, 3)?;
    let mut written = Vec::new();
    for (seed, id) in [(1u8, 1u32), (2, 2), (3, 3)] {
        let mut buf = page_with(512, seed)?;
        pager.write_page(PageId::new(id), &mut buf)?;
        written.push((id, buf));
    }
    for (id, expected) in &written {
        let mut buf = vec![0u8; 512];
        pager.read_page(PageId::new(*id), &mut buf)?;
        ensure!(&buf == expected, "page {id} was overwritten");
    }
    Ok(())
}

#[test]
fn rewriting_a_page_replaces_it() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 512, 1)?;
    let id = PageId::new(1);
    pager.write_page(id, &mut page_with(512, 1)?)?;
    let mut second = page_with(512, 2)?;
    pager.write_page(id, &mut second)?;
    let mut buf = vec![0u8; 512];
    pager.read_page(id, &mut buf)?;
    ensure!(buf == second, "read returned the old contents");
    Ok(())
}

#[test]
fn writes_do_not_change_page_count_or_file_length() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 512, 2)?;
    pager.write_page(PageId::new(2), &mut page_with(512, 9)?)?;
    ensure!(pager.page_count() == 3, "page_count changed");
    ensure!(file_len(&path)? == 1536, "file length changed");
    Ok(())
}

#[test]
fn data_survives_reopen() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 4096, 2)?;
    let mut first = page_with(4096, 0x11)?;
    let mut second = page_with(4096, 0x22)?;
    pager.write_page(PageId::new(1), &mut first)?;
    pager.write_page(PageId::new(2), &mut second)?;
    pager.sync()?;
    drop(pager);

    let pager = Pager::open(&path)?;
    let mut buf = vec![0u8; 4096];
    pager.read_page(PageId::new(1), &mut buf)?;
    ensure!(buf == first, "page 1 changed after reopen");
    pager.read_page(PageId::new(2), &mut buf)?;
    ensure!(buf == second, "page 2 changed after reopen");
    Ok(())
}

#[test]
fn page_0_can_be_read() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let pager = create(&path, 512)?;
    let mut buf = vec![0u8; 512];
    pager.read_page(PageId::HEADER, &mut buf)?;
    let header = FileHeader::decode(&buf)?;
    ensure!(header.page_count == 1, "got {header:?}");
    Ok(())
}

#[test]
fn reading_a_corrupted_page_reports_its_id() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 512, 2)?;
    pager.write_page(PageId::new(2), &mut page_with(512, 7)?)?;
    flip_byte(&path, 1324)?; // byte 300 of page 2
    let mut buf = vec![0u8; 512];
    let err = pager
        .read_page(PageId::new(2), &mut buf)
        .err()
        .context("read of a corrupted page succeeded")?;
    ensure!(
        matches!(
            err,
            PagerError::Corrupted {
                page,
                source: PageError::ChecksumMismatch { .. }
            } if page == PageId::new(2)
        ),
        "got {err:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Invalid arguments
// ---------------------------------------------------------------------------

#[test]
fn writing_page_0_is_rejected() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create(&path, 512)?;
    let before = fs::read(&path)?;
    let err = pager
        .write_page(PageId::HEADER, &mut page_with(512, 1)?)
        .err()
        .context("write to page 0 succeeded")?;
    ensure!(
        matches!(err, PagerError::ReservedPage { page } if page == PageId::HEADER),
        "got {err:?}"
    );
    ensure!(fs::read(&path)? == before, "the file changed");
    Ok(())
}

#[test]
fn pages_past_the_end_are_out_of_bounds() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 512, 2)?;
    for raw in [3, 4, u32::MAX] {
        let id = PageId::new(raw);
        let mut buf = page_with(512, 1)?;
        let write = pager.write_page(id, &mut buf).err();
        ensure!(
            matches!(write, Some(PagerError::PageOutOfBounds { page, page_count: 3 }) if page == id),
            "write {raw}: got {write:?}"
        );
        let read = pager.read_page(id, &mut buf).err();
        ensure!(
            matches!(read, Some(PagerError::PageOutOfBounds { page, page_count: 3 }) if page == id),
            "read {raw}: got {read:?}"
        );
    }
    ensure!(
        file_len(&path)? == 1536,
        "an out-of-bounds write grew the file"
    );
    Ok(())
}

#[test]
fn buffers_of_the_wrong_size_are_rejected() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 512, 1)?;
    let before = fs::read(&path)?;
    for len in [0, 16, 511, 513, 4096] {
        let mut buf = vec![0u8; len];
        let write = pager.write_page(PageId::new(1), &mut buf).err();
        ensure!(
            matches!(write, Some(PagerError::BufferSize { expected: 512, actual }) if actual == len),
            "write with {len} bytes: got {write:?}"
        );
        let read = pager.read_page(PageId::new(1), &mut buf).err();
        ensure!(
            matches!(read, Some(PagerError::BufferSize { expected: 512, actual }) if actual == len),
            "read with {len} bytes: got {read:?}"
        );
    }
    ensure!(fs::read(&path)? == before, "the file changed");
    Ok(())
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

proptest! {
    // Each case creates a file, so keep the number of cases small.
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn random_pages_round_trip_through_reopen(
        bodies in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 512),
            1..8,
        ),
    ) {
        let run = || -> anyhow::Result<()> {
            let (_dir, path) = temp_db()?;
            let count = u32::try_from(bodies.len())?;
            let mut pager = create_with_pages(&path, 512, count)?;
            let mut written = Vec::new();
            for (id, body) in (1..=count).zip(&bodies) {
                let mut buf = body.clone();
                pager.write_page(PageId::new(id), &mut buf)?;
                written.push(buf);
            }
            drop(pager);

            let pager = Pager::open(&path)?;
            ensure!(pager.page_count() == count.saturating_add(1), "page_count");
            for (id, expected) in (1..=count).zip(&written) {
                let mut buf = vec![0u8; 512];
                pager.read_page(PageId::new(id), &mut buf)?;
                ensure!(&buf == expected, "page {id} changed");
            }
            Ok(())
        };
        run().map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
