//! Step 7 (TDD): free list for page reuse (ADR 0003).
//!
//! Freed pages form a singly linked list (LIFO). The file header holds the
//! head (`freelist_head`, offset 36) and the length (`freelist_count`, offset
//! 40). Each free page links to the next one:
//!
//! | Offset | Size | Field                                  |
//! |--------|------|----------------------------------------|
//! | 0      | 16   | common header, page type 2 (Free)      |
//! | 16     | 4    | `next_free: u32`, 0 = end of the list  |
//! | 20     | rest | zero                                   |
//!
//! API under test:
//!
//! Module `storage::pager::freelist` (pure, no I/O):
//!
//! - `encode_free_page(&mut [u8], Option<PageId>) -> Result<(), PageError>`:
//!   turns the buffer into a free page that links to `next`. Zeroes every
//!   byte from offset 4 to the end, sets the page type to `Free`, and writes
//!   `next_free` at offset 16 (`None` becomes 0). The LSN stays 0. Never
//!   touches the checksum `[0, 4)`: the pager stamps it. A buffer shorter
//!   than 20 bytes gives `OutOfBounds` and is not modified.
//! - `decode_free_page(&[u8]) -> Result<Option<PageId>, PageError>`: checks
//!   that the page type is `Free` (`UnknownPageType` or `UnexpectedPageType`
//!   otherwise) and returns `next_free` (0 becomes `None`). A buffer shorter
//!   than 20 bytes gives `OutOfBounds`. Does not verify the checksum.
//!
//! `Pager` (module `storage::pager::disk`):
//!
//! - `free_page(&mut self, PageId) -> Result<(), PagerError>`: pushes the
//!   page onto the free list. Writes the page first, as a free page linked to
//!   the current head, then the file header with the page as the new head and
//!   `freelist_count + 1` (ADR 0003). Does not sync. Page 0 gives
//!   `ReservedPage` and an id `>= page_count` gives `PageOutOfBounds`; in both
//!   cases nothing is written. It does not read the page, so it cannot detect
//!   a double free: freeing a page twice links it to itself, and
//!   `verify_freelist` reports the cycle.
//! - `free_page_count(&self) -> u32`: the `freelist_count` of the header.
//! - `allocate_page`: if the free list is not empty, pops the head instead of
//!   growing the file. It reads the head page (verifying its checksum),
//!   decodes `next_free`, and writes the file header with `next_free` as the
//!   new head and `freelist_count - 1`. If the list is empty, it grows the
//!   file as in step 6. Errors while popping, with the file and the pager left
//!   unchanged:
//!   - the head fails its checksum: `Corrupted { page: head, source }`
//!   - the head is not a free page: `FreelistCorrupted { page: head, .. }`
//!   - `next_free` is `>= page_count`: `FreelistCorrupted { page: head, .. }`
//!   - `next_free` disagrees with the new count (count reaches 0 but
//!     `next_free` is set, or count stays above 0 but `next_free` is 0):
//!     `FreelistCorrupted { page: head, .. }`
//! - `verify_freelist(&self) -> Result<(), PagerError>`: walks the list from
//!   the head. Succeeds only if the list has exactly `freelist_count` pages,
//!   each one in `[1, page_count)`, each one a valid free page, and no page
//!   appears twice. A checksum failure gives `Corrupted { page, source }`.
//!   Any other problem gives `FreelistCorrupted { page, reason }`; when the
//!   problem is a bad page type or a `next_free` past the end of the file,
//!   `page` is the page that holds it. Never loops forever: it follows at most
//!   `freelist_count` links.
//!
//! New `PagerError` variant:
//!
//! - `FreelistCorrupted { page: PageId, reason: &'static str }`.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, ensure};
use proptest::prelude::*;
use proptest::sample::Index;
use storage_engine_rs::storage::pager::crc32::crc32;
use storage_engine_rs::storage::pager::disk::{Pager, PagerOptions};
use storage_engine_rs::storage::pager::error::{PageError, PagerError};
use storage_engine_rs::storage::pager::file_header::FileHeader;
use storage_engine_rs::storage::pager::freelist::{decode_free_page, encode_free_page};
use storage_engine_rs::storage::pager::page::{
    PageType, page_lsn, page_type, set_page_lsn, set_page_type,
};
use storage_engine_rs::storage::pager::types::{PageId, PageSize};
use tempfile::TempDir;

/// Page sizes allowed by ADR 0001 that the tests cover: smallest, default, largest.
const SIZES: [u32; 3] = [512, 4096, 65536];

// Offsets written here independently of the code (ADR 0003).
const OFF_FREELIST_COUNT: usize = 40;
const OFF_NEXT_FREE: usize = 16;
/// Smallest buffer that holds a free page: common header plus `next_free`.
const FREE_PAGE_END: usize = 20;

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

/// Creates a database with 512-byte pages and allocates `extra` pages, so
/// `page_count` is `extra + 1`.
fn create_with_pages(path: &Path, extra: u32) -> anyhow::Result<Pager> {
    let mut pager = Pager::create(
        path,
        PagerOptions {
            page_size: PageSize::new(512)?,
        },
    )?;
    for _ in 0..extra {
        pager.allocate_page()?;
    }
    Ok(pager)
}

/// Frees each page in `ids`, in order.
fn free_all(pager: &mut Pager, ids: &[u32]) -> anyhow::Result<()> {
    for &id in ids {
        pager.free_page(PageId::new(id))?;
    }
    Ok(())
}

/// A 512-byte page of type Slotted with a body derived from `seed`.
fn slotted_page(seed: u8) -> anyhow::Result<Vec<u8>> {
    let mut buf: Vec<u8> = (0..512usize)
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

/// Byte offset of `offset` inside 512-byte page `id`.
fn file_offset(id: u32, offset: usize) -> anyhow::Result<u64> {
    u64::from(id)
        .checked_mul(512)
        .and_then(|start| start.checked_add(u64::try_from(offset).ok()?))
        .context("offset overflow")
}

/// Overwrites bytes of the file at `path`, starting at `offset`, without the
/// pager. Used to simulate corruption.
fn patch(path: &Path, offset: u64, bytes: &[u8]) -> anyhow::Result<()> {
    use std::os::unix::fs::FileExt;
    let file = OpenOptions::new().write(true).open(path)?;
    file.write_all_at(bytes, offset)?;
    Ok(())
}

/// Reads 512-byte page `id` straight from the file, without the pager.
fn raw_page(path: &Path, id: u32) -> anyhow::Result<Vec<u8>> {
    let data = fs::read(path)?;
    let start = usize::try_from(file_offset(id, 0)?)?;
    let end = start.checked_add(512).context("offset overflow")?;
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

/// Writes `value` as a little-endian u32 at `offset` inside page `id`, then
/// recomputes the page checksum so that only the field looks wrong.
fn set_u32_and_restamp(path: &Path, id: u32, offset: usize, value: u32) -> anyhow::Result<()> {
    patch(path, file_offset(id, offset)?, &value.to_le_bytes())?;
    let page = raw_page(path, id)?;
    let crc = crc32(page.get(4..).context("body")?);
    patch(path, file_offset(id, 0)?, &crc.to_le_bytes())
}

/// Decodes the file header straight from page 0 of the file.
fn header_on_disk(path: &Path) -> anyhow::Result<FileHeader> {
    Ok(FileHeader::decode(&raw_page(path, 0)?)?)
}

/// Allocates and returns the error, failing the test if `allocate_page` succeeds.
fn allocate_err(pager: &mut Pager) -> anyhow::Result<PagerError> {
    pager
        .allocate_page()
        .err()
        .context("allocate succeeded but an error was expected")
}

/// Verifies the free list and returns the error, failing the test if it passes.
fn verify_err(pager: &Pager) -> anyhow::Result<PagerError> {
    pager
        .verify_freelist()
        .err()
        .context("verify_freelist succeeded but an error was expected")
}

// ---------------------------------------------------------------------------
// encode_free_page and decode_free_page
// ---------------------------------------------------------------------------

#[test]
fn free_page_round_trips_for_every_size() -> anyhow::Result<()> {
    for size in SIZES {
        let len = usize::try_from(size)?;
        for next in [None, Some(PageId::new(1)), Some(PageId::new(u32::MAX))] {
            let mut buf = vec![0xAAu8; len];
            encode_free_page(&mut buf, next)?;
            ensure!(
                decode_free_page(&buf)? == next,
                "size {size}, next {next:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn encode_writes_the_adr_layout() -> anyhow::Result<()> {
    let mut buf = vec![0xAAu8; 512];
    encode_free_page(&mut buf, Some(PageId::new(0x0102_0304)))?;
    ensure!(
        buf.get(0..4) == Some(&[0xAA; 4][..]),
        "the checksum bytes changed"
    );
    ensure!(buf.get(4) == Some(&2), "page type byte is not 2 (Free)");
    ensure!(page_lsn(&buf)? == 0, "LSN is not 0");
    ensure!(
        buf.get(OFF_NEXT_FREE..FREE_PAGE_END) == Some(&[0x04, 0x03, 0x02, 0x01][..]),
        "next_free is not little-endian at offset 16"
    );
    let reserved = buf
        .get(5..8)
        .context("reserved")?
        .iter()
        .chain(buf.get(FREE_PAGE_END..).context("body")?);
    ensure!(
        reserved.into_iter().all(|&b| b == 0),
        "reserved bytes and the rest of the page must be zero"
    );
    Ok(())
}

#[test]
fn encode_writes_0_for_the_end_of_the_list() -> anyhow::Result<()> {
    let mut buf = vec![0xAAu8; 512];
    encode_free_page(&mut buf, None)?;
    ensure!(
        buf.get(OFF_NEXT_FREE..FREE_PAGE_END) == Some(&[0u8; 4][..]),
        "None must be stored as 0"
    );
    Ok(())
}

#[test]
fn decode_rejects_pages_that_are_not_free() -> anyhow::Result<()> {
    let slotted = slotted_page(1)?;
    let err = decode_free_page(&slotted)
        .err()
        .context("a slotted page decoded as free")?;
    ensure!(
        err == PageError::UnexpectedPageType {
            expected: PageType::Free,
            found: PageType::Slotted,
        },
        "got {err:?}"
    );

    let zeros = vec![0u8; 512];
    let err = decode_free_page(&zeros)
        .err()
        .context("a zero page decoded as free")?;
    ensure!(
        err == PageError::UnknownPageType { value: 0 },
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn short_buffers_are_out_of_bounds() -> anyhow::Result<()> {
    let short = FREE_PAGE_END.checked_sub(1).context("underflow")?;

    let mut buf = vec![0xAAu8; short];
    let err = encode_free_page(&mut buf, Some(PageId::new(3)))
        .err()
        .context("encode into 19 bytes succeeded")?;
    ensure!(
        matches!(err, PageError::OutOfBounds { .. }),
        "encode: got {err:?}"
    );
    ensure!(buf.iter().all(|&b| b == 0xAA), "encode modified the buffer");

    let mut buf = vec![0u8; short];
    set_page_type(&mut buf, PageType::Free)?;
    let err = decode_free_page(&buf)
        .err()
        .context("decode of 19 bytes succeeded")?;
    ensure!(
        matches!(err, PageError::OutOfBounds { .. }),
        "decode: got {err:?}"
    );

    // Exactly 20 bytes is enough.
    let mut buf = vec![0u8; FREE_PAGE_END];
    encode_free_page(&mut buf, Some(PageId::new(3)))?;
    ensure!(
        decode_free_page(&buf)? == Some(PageId::new(3)),
        "20-byte round trip"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// free_page and allocate_page
// ---------------------------------------------------------------------------

#[test]
fn a_new_database_has_an_empty_free_list() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let pager = create_with_pages(&path, 0)?;
    ensure!(pager.free_page_count() == 0, "free_page_count");
    pager.verify_freelist()?;
    Ok(())
}

#[test]
fn a_freed_page_is_reused_and_the_file_does_not_grow() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 3)?;
    pager.free_page(PageId::new(2))?;
    ensure!(pager.free_page_count() == 1, "free_page_count after free");
    ensure!(pager.page_count() == 4, "free changed page_count");

    let id = pager.allocate_page()?;
    ensure!(id == PageId::new(2), "allocate returned page {id}, not 2");
    ensure!(
        pager.free_page_count() == 0,
        "free_page_count after allocate"
    );
    ensure!(pager.page_count() == 4, "allocate grew page_count");
    ensure!(file_len(&path)? == 2048, "the file grew");

    // The reused page works like any other page.
    let mut written = slotted_page(7)?;
    pager.write_page(id, &mut written)?;
    let mut read = vec![0u8; 512];
    pager.read_page(id, &mut read)?;
    ensure!(read == written, "reused page round trip");
    Ok(())
}

#[test]
fn allocation_is_lifo_and_then_grows_the_file() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 8)?;
    free_all(&mut pager, &[3, 5, 7])?;
    ensure!(pager.free_page_count() == 3, "free_page_count");

    for expected in [7, 5, 3] {
        let id = pager.allocate_page()?;
        ensure!(
            id == PageId::new(expected),
            "got page {id}, expected {expected}"
        );
    }
    ensure!(file_len(&path)? == 9 * 512, "reusing pages grew the file");

    let id = pager.allocate_page()?;
    ensure!(
        id == PageId::new(9),
        "empty list must grow the file, got {id}"
    );
    ensure!(pager.page_count() == 10, "page_count after growing");
    Ok(())
}

#[test]
fn free_writes_the_free_page_and_the_header_to_disk() -> anyhow::Result<()> {
    // No sync and no reopen: both writes must already be in the file.
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 4)?;
    free_all(&mut pager, &[2, 4])?;

    let header = header_on_disk(&path)?;
    ensure!(
        header.freelist_head == Some(PageId::new(4)),
        "head on disk: {:?}",
        header.freelist_head
    );
    ensure!(header.freelist_count == 2, "count on disk");
    ensure!(header.page_count == 5, "page_count on disk");

    let page_4 = raw_page(&path, 4)?;
    ensure!(raw_checksum_ok(&page_4)?, "page 4 checksum not stamped");
    ensure!(
        decode_free_page(&page_4)? == Some(PageId::new(2)),
        "page 4 must link to page 2"
    );
    let page_2 = raw_page(&path, 2)?;
    ensure!(raw_checksum_ok(&page_2)?, "page 2 checksum not stamped");
    ensure!(
        decode_free_page(&page_2)?.is_none(),
        "page 2 is the end of the list"
    );
    ensure!(
        page_2
            .get(FREE_PAGE_END..)
            .context("body")?
            .iter()
            .all(|&b| b == 0),
        "a free page must not keep its old contents"
    );
    Ok(())
}

#[test]
fn a_freed_page_can_be_read_as_a_free_page() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 2)?;
    pager.write_page(PageId::new(1), &mut slotted_page(1)?)?;
    pager.free_page(PageId::new(1))?;
    let mut buf = vec![0u8; 512];
    pager.read_page(PageId::new(1), &mut buf)?;
    ensure!(page_type(&buf)? == PageType::Free, "page type after free");
    Ok(())
}

#[test]
fn the_free_list_persists_after_reopen() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 5)?;
    free_all(&mut pager, &[2, 4])?;
    pager.sync()?;
    drop(pager);

    let mut pager = Pager::open(&path)?;
    ensure!(pager.free_page_count() == 2, "free_page_count after reopen");
    pager.verify_freelist()?;
    for expected in [4, 2, 6] {
        let id = pager.allocate_page()?;
        ensure!(
            id == PageId::new(expected),
            "got page {id}, expected {expected}"
        );
    }
    Ok(())
}

#[test]
fn freeing_page_0_is_rejected() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 2)?;
    let before = fs::read(&path)?;
    let err = pager
        .free_page(PageId::HEADER)
        .err()
        .context("freeing page 0 succeeded")?;
    ensure!(
        matches!(err, PagerError::ReservedPage { page } if page == PageId::HEADER),
        "got {err:?}"
    );
    ensure!(fs::read(&path)? == before, "the file changed");
    ensure!(pager.free_page_count() == 0, "free_page_count changed");
    Ok(())
}

#[test]
fn freeing_a_page_past_the_end_is_rejected() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 2)?;
    let before = fs::read(&path)?;
    for raw in [3, 4, u32::MAX] {
        let id = PageId::new(raw);
        let err = pager
            .free_page(id)
            .err()
            .context("freeing a page past the end succeeded")?;
        ensure!(
            matches!(err, PagerError::PageOutOfBounds { page, page_count: 3 } if page == id),
            "free {raw}: got {err:?}"
        );
    }
    ensure!(fs::read(&path)? == before, "the file changed");
    ensure!(pager.free_page_count() == 0, "free_page_count changed");
    Ok(())
}

// ---------------------------------------------------------------------------
// allocate_page with a corrupted free list
// ---------------------------------------------------------------------------

#[test]
fn allocate_rejects_a_head_that_is_not_a_free_page() -> anyhow::Result<()> {
    // Use after free: the caller writes a page it already freed.
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 3)?;
    pager.free_page(PageId::new(3))?;
    pager.write_page(PageId::new(3), &mut slotted_page(1)?)?;
    let before = fs::read(&path)?;

    let err = allocate_err(&mut pager)?;
    ensure!(
        matches!(err, PagerError::FreelistCorrupted { page, .. } if page == PageId::new(3)),
        "got {err:?}"
    );
    ensure!(fs::read(&path)? == before, "the file changed");
    ensure!(pager.free_page_count() == 1, "free_page_count changed");
    ensure!(pager.page_count() == 4, "page_count changed");

    // The pager state did not move: a second try fails the same way.
    let again = allocate_err(&mut pager)?;
    ensure!(
        matches!(again, PagerError::FreelistCorrupted { page, .. } if page == PageId::new(3)),
        "second try: got {again:?}"
    );
    Ok(())
}

#[test]
fn allocate_reports_a_head_with_a_bad_checksum() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 3)?;
    pager.free_page(PageId::new(3))?;
    patch(&path, file_offset(3, 100)?, &[0xFF])?;
    let before = fs::read(&path)?;

    let err = allocate_err(&mut pager)?;
    ensure!(
        matches!(
            err,
            PagerError::Corrupted {
                page,
                source: PageError::ChecksumMismatch { .. }
            } if page == PageId::new(3)
        ),
        "got {err:?}"
    );
    ensure!(fs::read(&path)? == before, "the file changed");
    ensure!(pager.free_page_count() == 1, "free_page_count changed");
    Ok(())
}

#[test]
fn allocate_rejects_a_next_page_past_the_end() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 3)?;
    free_all(&mut pager, &[2, 3])?;
    set_u32_and_restamp(&path, 3, OFF_NEXT_FREE, 99)?;
    let before = fs::read(&path)?;

    let err = allocate_err(&mut pager)?;
    ensure!(
        matches!(err, PagerError::FreelistCorrupted { page, .. } if page == PageId::new(3)),
        "got {err:?}"
    );
    ensure!(fs::read(&path)? == before, "the file changed");
    ensure!(pager.free_page_count() == 2, "free_page_count changed");
    Ok(())
}

#[test]
fn allocate_rejects_a_list_longer_than_the_count() -> anyhow::Result<()> {
    // List 3 -> 2, but the header says 1 page: popping 3 would leave count 0
    // with a head of 2, a header that `open` rejects.
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 3)?;
    free_all(&mut pager, &[2, 3])?;
    drop(pager);
    set_u32_and_restamp(&path, 0, OFF_FREELIST_COUNT, 1)?;

    let mut pager = Pager::open(&path)?;
    let err = allocate_err(&mut pager)?;
    ensure!(
        matches!(err, PagerError::FreelistCorrupted { page, .. } if page == PageId::new(3)),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn allocate_rejects_a_list_shorter_than_the_count() -> anyhow::Result<()> {
    // List 3 -> 2, but the header says 3 pages: popping 2 would leave count 1
    // with an empty list.
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 4)?;
    free_all(&mut pager, &[2, 3])?;
    drop(pager);
    set_u32_and_restamp(&path, 0, OFF_FREELIST_COUNT, 3)?;

    let mut pager = Pager::open(&path)?;
    ensure!(
        pager.allocate_page()? == PageId::new(3),
        "first pop is still valid"
    );
    let err = allocate_err(&mut pager)?;
    ensure!(
        matches!(err, PagerError::FreelistCorrupted { page, .. } if page == PageId::new(2)),
        "got {err:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// verify_freelist
// ---------------------------------------------------------------------------

#[test]
fn verify_accepts_a_valid_list() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 8)?;
    free_all(&mut pager, &[1, 4, 8, 2])?;
    pager.verify_freelist()?;
    pager.allocate_page()?;
    pager.verify_freelist()?;
    Ok(())
}

#[test]
fn verify_detects_a_double_free_without_looping_forever() -> anyhow::Result<()> {
    // Freeing 3 twice links it to itself: 3 -> 3, count 2.
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 3)?;
    free_all(&mut pager, &[3, 3])?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // If the receiver already gave up there is nobody left to tell.
        let _ = tx.send(pager.verify_freelist());
    });
    let result = rx
        .recv_timeout(Duration::from_secs(5))
        .context("verify_freelist did not return within 5 s: it loops on a cycle")?;
    let err = result.err().context("verify_freelist accepted a cycle")?;
    ensure!(
        matches!(err, PagerError::FreelistCorrupted { .. }),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn verify_detects_a_page_that_is_not_free() -> anyhow::Result<()> {
    // List 4 -> 3 -> 2, then page 3 is overwritten.
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 4)?;
    free_all(&mut pager, &[2, 3, 4])?;
    pager.write_page(PageId::new(3), &mut slotted_page(1)?)?;

    let err = verify_err(&pager)?;
    ensure!(
        matches!(err, PagerError::FreelistCorrupted { page, .. } if page == PageId::new(3)),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn verify_detects_a_next_page_past_the_end() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 4)?;
    free_all(&mut pager, &[2, 3, 4])?;
    drop(pager);
    set_u32_and_restamp(&path, 3, OFF_NEXT_FREE, 99)?;

    let pager = Pager::open(&path)?;
    let err = verify_err(&pager)?;
    ensure!(
        matches!(err, PagerError::FreelistCorrupted { page, .. } if page == PageId::new(3)),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn verify_reports_a_page_with_a_bad_checksum() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 4)?;
    free_all(&mut pager, &[2, 3, 4])?;
    patch(&path, file_offset(3, 100)?, &[0xFF])?;

    let err = verify_err(&pager)?;
    ensure!(
        matches!(
            err,
            PagerError::Corrupted {
                page,
                source: PageError::ChecksumMismatch { .. }
            } if page == PageId::new(3)
        ),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn verify_detects_a_list_longer_than_the_count() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 3)?;
    free_all(&mut pager, &[2, 3])?;
    drop(pager);
    set_u32_and_restamp(&path, 0, OFF_FREELIST_COUNT, 1)?;

    let pager = Pager::open(&path)?;
    let err = verify_err(&pager)?;
    ensure!(
        matches!(err, PagerError::FreelistCorrupted { .. }),
        "got {err:?}"
    );
    Ok(())
}

#[test]
fn verify_detects_a_list_shorter_than_the_count() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 4)?;
    free_all(&mut pager, &[2, 3])?;
    drop(pager);
    set_u32_and_restamp(&path, 0, OFF_FREELIST_COUNT, 3)?;

    let pager = Pager::open(&path)?;
    let err = verify_err(&pager)?;
    ensure!(
        matches!(err, PagerError::FreelistCorrupted { .. }),
        "got {err:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

/// One step of a random workload: allocate a page, or free one of the pages
/// that are in use (chosen by the index).
#[derive(Debug, Clone)]
enum Op {
    Allocate,
    Free(Index),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![Just(Op::Allocate), any::<Index>().prop_map(Op::Free)]
}

/// Runs `ops` against a pager and a simple model (a stack of free ids and the
/// next id past the end of the file), then checks that both agree, before and
/// after a reopen.
fn run_against_model(ops: &[Op]) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = create_with_pages(&path, 0)?;
    let mut free: Vec<u32> = Vec::new();
    let mut in_use: Vec<u32> = Vec::new();
    let mut next_new: u32 = 1;

    for op in ops {
        match op {
            Op::Allocate => {
                let expected = if let Some(id) = free.pop() {
                    id
                } else {
                    let id = next_new;
                    next_new = next_new.checked_add(1).context("overflow")?;
                    id
                };
                let id = pager.allocate_page()?;
                ensure!(
                    id == PageId::new(expected),
                    "allocate: got {id}, expected {expected}"
                );
                in_use.push(expected);
            }
            Op::Free(index) => {
                if in_use.is_empty() {
                    continue;
                }
                let id = in_use.swap_remove(index.index(in_use.len()));
                pager.free_page(PageId::new(id))?;
                free.push(id);
            }
        }
    }

    let free_count = u32::try_from(free.len())?;
    ensure!(pager.page_count() == next_new, "page_count");
    ensure!(pager.free_page_count() == free_count, "free_page_count");
    pager.verify_freelist()?;
    ensure!(
        file_len(&path)? == u64::from(next_new).checked_mul(512).context("overflow")?,
        "file length"
    );
    drop(pager);

    let mut pager = Pager::open(&path)?;
    ensure!(
        pager.free_page_count() == free_count,
        "free_page_count after reopen"
    );
    pager.verify_freelist()?;
    while let Some(expected) = free.pop() {
        let id = pager.allocate_page()?;
        ensure!(
            id == PageId::new(expected),
            "drain: got {id}, expected {expected}"
        );
    }
    ensure!(
        pager.free_page_count() == 0,
        "free list not empty after draining"
    );
    ensure!(pager.page_count() == next_new, "draining grew the file");
    Ok(())
}

proptest! {
    // Each case creates a file, so keep the number of cases small.
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn free_and_allocate_match_a_stack_model(ops in prop::collection::vec(op(), 1..40)) {
        run_against_model(&ops).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
