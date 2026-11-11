//! Step 5 (TDD): database file header, stored in page 0 (ADR 0003).
//!
//! Page 0 starts with the common page header (step 4) with page type
//! `FileHeader`. The file header follows it:
//!
//! | Offset | Size | Field                                                    |
//! |--------|------|----------------------------------------------------------|
//! | 16     | 8    | magic: `STENGDB\0`                                       |
//! | 24     | 2    | `format_version`: u16 = 1                                |
//! | 26     | 2    | reserved                                                 |
//! | 28     | 4    | `page_size`: u32 (ADR 0001)                              |
//! | 32     | 4    | `page_count`: u32, >= 1, includes page 0                 |
//! | 36     | 4    | `freelist_head`: u32, 0 = empty, otherwise < `page_count` |
//! | 40     | 4    | `freelist_count`: u32, 0 if and only if head is 0, <= `page_count` - 1 |
//! | 44     | rest | reserved                                                 |
//!
//! All integers are little-endian.
//!
//! API under test (module `storage::pager::file_header`):
//!
//! - `MAGIC: [u8; 8] = *b"STENGDB\0"`
//! - `FORMAT_VERSION: u16 = 1`
//! - `FILE_HEADER_END: usize = 44`: first byte after the file header
//! - `FileHeader` with public fields `page_size: PageSize`, `page_count: u32`,
//!   `freelist_head: Option<PageId>`, `freelist_count: u32`. Needs `Debug`,
//!   `Clone`, `Copy`, `PartialEq`, `Eq`.
//! - `FileHeader::new(PageSize) -> FileHeader`: header of a new database, with
//!   only page 0 and an empty free list.
//! - `FileHeader::encode(&self, &mut [u8]) -> Result<(), PageError>`:
//!   - zeroes every byte from offset 4 to the end of the buffer, then writes
//!     page type `FileHeader` and the fields;
//!   - never touches the checksum `[0, 4)`, because the pager stamps it later;
//!   - a buffer shorter than `FILE_HEADER_END` gives `OutOfBounds` and is left
//!     unchanged.
//! - `FileHeader::decode(&[u8]) -> Result<FileHeader, PageError>`:
//!   - works on any buffer of at least `FILE_HEADER_END` bytes, because
//!     `Pager::open` reads only a 512-byte prefix before it knows the page size;
//!   - does not verify the checksum (the pager does that with the full page) and
//!     ignores the LSN and the reserved bytes;
//!   - checks in this order, and returns the first failure:
//!     1. buffer length: `OutOfBounds`
//!     2. magic: `BadMagic`
//!     3. version: `UnsupportedVersion`
//!     4. page size: `InvalidPageSize` (from `PageSize::new`)
//!     5. page type: `UnknownPageType` or `UnexpectedPageType`
//!     6. rules between fields: `CorruptHeader`
//!
//! New `PageError` variants:
//!
//! - `BadMagic { found: [u8; 8] }`: the bytes read instead of the magic.
//! - `UnsupportedVersion { version: u16 }`
//! - `CorruptHeader { reason: &'static str }`: the reason text is free; the
//!   tests do not check it.
//!
//! Why this order: a file that is not a database at all should say so (bad
//! magic) before anything else. A file from a newer version may use a different
//! layout, so no field after the version can be trusted until the version is
//! known.

use anyhow::{Context, ensure};
use proptest::prelude::*;
use storage_engine_rs::storage::pager::codec::{write_bytes, write_u8, write_u16, write_u32};
use storage_engine_rs::storage::pager::error::PageError;
use storage_engine_rs::storage::pager::file_header::{
    FILE_HEADER_END, FORMAT_VERSION, FileHeader, MAGIC,
};
use storage_engine_rs::storage::pager::page::{PageType, page_lsn, page_type, set_page_lsn};
use storage_engine_rs::storage::pager::types::{PageId, PageSize};

// Field offsets, written here independently of the implementation.
const OFF_PAGE_TYPE: usize = 4;
const OFF_MAGIC: usize = 16;
const OFF_VERSION: usize = 24;
const OFF_RESERVED: usize = 26;
const OFF_PAGE_SIZE: usize = 28;
const OFF_PAGE_COUNT: usize = 32;
const OFF_FREELIST_HEAD: usize = 36;
const OFF_FREELIST_COUNT: usize = 40;

/// Page sizes allowed by ADR 0001 that the tests cover: smallest, default, largest.
const SIZES: [u32; 3] = [512, 4096, 65536];

/// A valid header with a non-empty free list: 7 pages, free list 5 -> ... (2 pages).
fn sample() -> anyhow::Result<FileHeader> {
    Ok(FileHeader {
        page_size: PageSize::new(4096)?,
        page_count: 7,
        freelist_head: Some(PageId::new(5)),
        freelist_count: 2,
    })
}

/// Encodes `header` into a zeroed buffer as large as its page size.
fn encoded(header: &FileHeader) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; header.page_size.as_usize()];
    header.encode(&mut buf)?;
    Ok(buf)
}

/// Encodes `sample()` into a 4096-byte buffer.
fn sample_page() -> anyhow::Result<Vec<u8>> {
    encoded(&sample()?)
}

/// Decodes `buf` and checks that it fails with `CorruptHeader`.
fn assert_corrupt(buf: &[u8], case: &str) -> anyhow::Result<()> {
    let result = FileHeader::decode(buf);
    ensure!(
        matches!(result, Err(PageError::CorruptHeader { .. })),
        "{case}: expected CorruptHeader, got {result:?}"
    );
    Ok(())
}

/// Builds a page whose page count and free list fields hold raw values.
/// Everything else (magic, version, page size, type) is valid.
fn page_with_counts(page_count: u32, head: u32, count: u32) -> anyhow::Result<Vec<u8>> {
    let mut buf = sample_page()?;
    write_u32(&mut buf, OFF_PAGE_COUNT, page_count)?;
    write_u32(&mut buf, OFF_FREELIST_HEAD, head)?;
    write_u32(&mut buf, OFF_FREELIST_COUNT, count)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Constants and FileHeader::new
// ---------------------------------------------------------------------------

#[test]
fn constants_match_adr() {
    assert_eq!(&MAGIC, b"STENGDB\0");
    assert_eq!(FORMAT_VERSION, 1);
    assert_eq!(FILE_HEADER_END, 44);
}

#[test]
fn new_header_describes_an_empty_database() -> anyhow::Result<()> {
    let page_size = PageSize::new(512)?;
    let header = FileHeader::new(page_size);
    ensure!(header.page_size == page_size, "page size changed");
    ensure!(header.page_count == 1, "only page 0 exists in a new file");
    ensure!(header.freelist_head.is_none(), "free list must be empty");
    ensure!(header.freelist_count == 0, "free list count must be 0");
    Ok(())
}

// ---------------------------------------------------------------------------
// encode: exact bytes
// ---------------------------------------------------------------------------

#[test]
fn encode_writes_every_field_at_its_offset() -> anyhow::Result<()> {
    let buf = sample_page()?;
    let expect = |offset: usize, bytes: &[u8]| -> anyhow::Result<()> {
        let end = offset.checked_add(bytes.len()).context("offset overflow")?;
        let got = buf.get(offset..end).context("range outside buffer")?;
        ensure!(
            got == bytes,
            "at offset {offset}: got {got:02x?}, expected {bytes:02x?}"
        );
        Ok(())
    };
    expect(OFF_PAGE_TYPE, &[1])?; // FileHeader
    expect(5, &[0, 0, 0])?; // reserved
    expect(8, &[0; 8])?; // LSN
    expect(OFF_MAGIC, b"STENGDB\0")?;
    expect(OFF_VERSION, &[0x01, 0x00])?;
    expect(OFF_RESERVED, &[0x00, 0x00])?;
    expect(OFF_PAGE_SIZE, &[0x00, 0x10, 0x00, 0x00])?; // 4096
    expect(OFF_PAGE_COUNT, &[0x07, 0x00, 0x00, 0x00])?;
    expect(OFF_FREELIST_HEAD, &[0x05, 0x00, 0x00, 0x00])?;
    expect(OFF_FREELIST_COUNT, &[0x02, 0x00, 0x00, 0x00])?;
    Ok(())
}

#[test]
fn encode_writes_empty_free_list_as_zero() -> anyhow::Result<()> {
    let buf = encoded(&FileHeader::new(PageSize::new(512)?))?;
    let fields = buf.get(OFF_PAGE_COUNT..FILE_HEADER_END).context("fields")?;
    ensure!(
        fields == [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        "page_count 1, head 0, count 0 expected, got {fields:02x?}"
    );
    Ok(())
}

#[test]
fn encode_zeroes_everything_after_checksum_except_fields() -> anyhow::Result<()> {
    // Start from garbage: encode must not leave old bytes behind.
    let header = sample()?;
    let mut buf = vec![0xAAu8; header.page_size.as_usize()];
    header.encode(&mut buf)?;

    ensure!(page_lsn(&buf)? == 0, "the LSN must be zeroed");
    let tail = buf.get(FILE_HEADER_END..).context("tail")?;
    ensure!(
        tail.iter().all(|b| *b == 0),
        "bytes after the file header must be zero"
    );
    let reserved = buf.get(OFF_RESERVED..OFF_PAGE_SIZE).context("reserved")?;
    ensure!(reserved == [0, 0], "reserved bytes 26..28 must be zero");
    let common_reserved = buf.get(5..8).context("common reserved")?;
    ensure!(
        common_reserved == [0, 0, 0],
        "reserved bytes 5..8 must be zero"
    );
    Ok(())
}

#[test]
fn encode_leaves_checksum_untouched() -> anyhow::Result<()> {
    let header = sample()?;
    let mut buf = vec![0xAAu8; header.page_size.as_usize()];
    header.encode(&mut buf)?;
    let checksum = buf.get(0..4).context("checksum")?;
    ensure!(
        checksum == [0xAA; 4],
        "encode wrote the checksum: {checksum:02x?}"
    );
    Ok(())
}

#[test]
fn encode_sets_page_type() -> anyhow::Result<()> {
    let buf = sample_page()?;
    ensure!(page_type(&buf)? == PageType::FileHeader, "wrong page type");
    Ok(())
}

#[test]
fn encode_accepts_exactly_header_sized_buffer() -> anyhow::Result<()> {
    let header = sample()?;
    let mut buf = vec![0u8; FILE_HEADER_END];
    header.encode(&mut buf)?;
    ensure!(
        FileHeader::decode(&buf)? == header,
        "44-byte round trip failed"
    );
    Ok(())
}

#[test]
fn encode_rejects_short_buffers_without_writing() -> anyhow::Result<()> {
    let header = sample()?;
    for len in 0..FILE_HEADER_END {
        let mut buf = vec![0xAAu8; len];
        let result = header.encode(&mut buf);
        ensure!(
            matches!(result, Err(PageError::OutOfBounds { .. })),
            "len {len}: got {result:?}"
        );
        ensure!(
            buf.iter().all(|b| *b == 0xAA),
            "len {len}: a failed encode modified the buffer"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// decode: round trip and prefixes
// ---------------------------------------------------------------------------

#[test]
fn round_trip_for_every_page_size() -> anyhow::Result<()> {
    for size in SIZES {
        let header = FileHeader::new(PageSize::new(size)?);
        let decoded = FileHeader::decode(&encoded(&header)?)?;
        ensure!(decoded == header, "size {size}: got {decoded:?}");
    }
    Ok(())
}

#[test]
fn decode_works_on_a_prefix() -> anyhow::Result<()> {
    // Pager::open reads 512 bytes before it knows the real page size.
    let header = FileHeader {
        page_size: PageSize::new(65536)?,
        ..sample()?
    };
    let buf = encoded(&header)?;
    for len in [512, FILE_HEADER_END] {
        let prefix = buf.get(..len).context("prefix")?;
        let decoded = FileHeader::decode(prefix)?;
        ensure!(decoded == header, "prefix of {len} bytes: got {decoded:?}");
    }
    Ok(())
}

#[test]
fn decode_rejects_buffers_shorter_than_header() -> anyhow::Result<()> {
    let buf = sample_page()?;
    for len in 0..FILE_HEADER_END {
        let prefix = buf.get(..len).context("prefix")?;
        let result = FileHeader::decode(prefix);
        ensure!(
            matches!(result, Err(PageError::OutOfBounds { .. })),
            "len {len}: got {result:?}"
        );
    }
    Ok(())
}

#[test]
fn decode_ignores_checksum_lsn_and_reserved_bytes() -> anyhow::Result<()> {
    let header = sample()?;
    let mut buf = encoded(&header)?;
    write_bytes(&mut buf, 0, &[0xDE, 0xAD, 0xBE, 0xEF])?; // never stamped
    set_page_lsn(&mut buf, 99)?;
    write_bytes(&mut buf, 5, &[0xFF; 3])?;
    write_bytes(&mut buf, OFF_RESERVED, &[0xFF; 2])?;
    write_u8(&mut buf, FILE_HEADER_END, 0xFF)?;
    let decoded = FileHeader::decode(&buf)?;
    ensure!(decoded == header, "got {decoded:?}");
    Ok(())
}

// ---------------------------------------------------------------------------
// decode: one test per error
// ---------------------------------------------------------------------------

#[test]
fn bad_magic_reports_bytes_found() -> anyhow::Result<()> {
    let cases: [[u8; 8]; 4] = [
        [0; 8],        // zero-filled file
        *b"STENGDBX",  // last byte differs
        *b"stengdb\0", // wrong case
        *b"SQLite f",  // another database's file
    ];
    for found in cases {
        let mut buf = sample_page()?;
        write_bytes(&mut buf, OFF_MAGIC, &found)?;
        let result = FileHeader::decode(&buf);
        ensure!(
            result == Err(PageError::BadMagic { found }),
            "magic {found:02x?}: got {result:?}"
        );
    }
    Ok(())
}

#[test]
fn unsupported_version_is_rejected() -> anyhow::Result<()> {
    for version in [0, 2, u16::MAX] {
        let mut buf = sample_page()?;
        write_u16(&mut buf, OFF_VERSION, version)?;
        let result = FileHeader::decode(&buf);
        ensure!(
            result == Err(PageError::UnsupportedVersion { version }),
            "version {version}: got {result:?}"
        );
    }
    Ok(())
}

#[test]
fn invalid_page_size_is_rejected() -> anyhow::Result<()> {
    for size in [0, 256, 1000, 4095, 131_072, u32::MAX] {
        let mut buf = sample_page()?;
        write_u32(&mut buf, OFF_PAGE_SIZE, size)?;
        let result = FileHeader::decode(&buf);
        ensure!(
            result == Err(PageError::InvalidPageSize { size }),
            "page size {size}: got {result:?}"
        );
    }
    Ok(())
}

#[test]
fn unknown_page_type_is_rejected() -> anyhow::Result<()> {
    let mut buf = sample_page()?;
    write_u8(&mut buf, OFF_PAGE_TYPE, 0)?;
    let result = FileHeader::decode(&buf);
    ensure!(
        result == Err(PageError::UnknownPageType { value: 0 }),
        "got {result:?}"
    );
    Ok(())
}

#[test]
fn other_page_type_is_rejected() -> anyhow::Result<()> {
    let mut buf = sample_page()?;
    write_u8(&mut buf, OFF_PAGE_TYPE, u8::from(PageType::Free))?;
    let result = FileHeader::decode(&buf);
    ensure!(
        result
            == Err(PageError::UnexpectedPageType {
                expected: PageType::FileHeader,
                found: PageType::Free,
            }),
        "got {result:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// decode: rules between fields
// ---------------------------------------------------------------------------

#[test]
fn page_count_zero_is_corrupt() -> anyhow::Result<()> {
    // Page 0 always exists, so the count is at least 1.
    assert_corrupt(&page_with_counts(0, 0, 0)?, "page_count 0")
}

#[test]
fn freelist_head_out_of_range_is_corrupt() -> anyhow::Result<()> {
    assert_corrupt(&page_with_counts(7, 7, 1)?, "head == page_count")?;
    assert_corrupt(&page_with_counts(7, 8, 1)?, "head > page_count")?;
    assert_corrupt(&page_with_counts(7, u32::MAX, 1)?, "head u32::MAX")
}

#[test]
fn freelist_head_and_count_must_agree() -> anyhow::Result<()> {
    assert_corrupt(&page_with_counts(7, 3, 0)?, "head set, count 0")?;
    assert_corrupt(&page_with_counts(7, 0, 2)?, "head 0, count 2")
}

#[test]
fn freelist_count_cannot_exceed_free_pages() -> anyhow::Result<()> {
    // Page 0 is never free, so at most page_count - 1 pages are free.
    assert_corrupt(&page_with_counts(7, 3, 7)?, "count == page_count")?;
    assert_corrupt(&page_with_counts(7, 3, u32::MAX)?, "count u32::MAX")?;
    assert_corrupt(&page_with_counts(1, 0, 1)?, "only page 0, count 1")
}

#[test]
fn boundary_values_are_valid() -> anyhow::Result<()> {
    let cases = [
        (1, 0, 0), // only page 0
        (7, 6, 6), // every other page free, head is the last page
        (7, 1, 1), // smallest head
        (
            u32::MAX,
            u32::MAX.saturating_sub(1),
            u32::MAX.saturating_sub(1),
        ), // largest file
    ];
    for (page_count, head, count) in cases {
        let buf = page_with_counts(page_count, head, count)?;
        let decoded = FileHeader::decode(&buf)
            .with_context(|| format!("({page_count}, {head}, {count}) should be valid"))?;
        ensure!(decoded.page_count == page_count, "page_count changed");
        ensure!(
            decoded.freelist_head == PageId::from_disk(head),
            "freelist_head changed"
        );
        ensure!(decoded.freelist_count == count, "freelist_count changed");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// decode: check order
// ---------------------------------------------------------------------------

#[test]
fn magic_is_checked_before_version() -> anyhow::Result<()> {
    let mut buf = sample_page()?;
    write_bytes(&mut buf, OFF_MAGIC, &[0; 8])?;
    write_u16(&mut buf, OFF_VERSION, 2)?;
    let result = FileHeader::decode(&buf);
    ensure!(
        matches!(result, Err(PageError::BadMagic { .. })),
        "got {result:?}"
    );
    Ok(())
}

#[test]
fn version_is_checked_before_page_size() -> anyhow::Result<()> {
    let mut buf = sample_page()?;
    write_u16(&mut buf, OFF_VERSION, 2)?;
    write_u32(&mut buf, OFF_PAGE_SIZE, 1000)?;
    let result = FileHeader::decode(&buf);
    ensure!(
        result == Err(PageError::UnsupportedVersion { version: 2 }),
        "got {result:?}"
    );
    Ok(())
}

#[test]
fn page_size_is_checked_before_page_type() -> anyhow::Result<()> {
    let mut buf = sample_page()?;
    write_u32(&mut buf, OFF_PAGE_SIZE, 1000)?;
    write_u8(&mut buf, OFF_PAGE_TYPE, 0)?;
    let result = FileHeader::decode(&buf);
    ensure!(
        result == Err(PageError::InvalidPageSize { size: 1000 }),
        "got {result:?}"
    );
    Ok(())
}

#[test]
fn page_type_is_checked_before_field_rules() -> anyhow::Result<()> {
    let mut buf = page_with_counts(0, 0, 0)?;
    write_u8(&mut buf, OFF_PAGE_TYPE, u8::from(PageType::Slotted))?;
    let result = FileHeader::decode(&buf);
    ensure!(
        matches!(result, Err(PageError::UnexpectedPageType { .. })),
        "got {result:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

/// Any valid header: a page size from `SIZES`, at least one page, and a free
/// list that is either empty or has a head and a count in range.
fn any_valid_header() -> impl Strategy<Value = FileHeader> {
    (
        prop::sample::select(SIZES.to_vec()),
        1u32..=u32::MAX,
        any::<bool>(),
        any::<u32>(),
        any::<u32>(),
    )
        .prop_filter_map(
            "invalid page size",
            |(size, page_count, has_free, head_seed, count_seed)| {
                let page_size = PageSize::new(size).ok()?;
                // Pages 1..page_count can be free: there are page_count - 1 of them.
                let free_pages = page_count.saturating_sub(1);
                let (freelist_head, freelist_count) = if has_free && free_pages > 0 {
                    let head = head_seed.checked_rem(free_pages)?.saturating_add(1);
                    let count = count_seed.checked_rem(free_pages)?.saturating_add(1);
                    (Some(PageId::new(head)), count)
                } else {
                    (None, 0)
                };
                Some(FileHeader {
                    page_size,
                    page_count,
                    freelist_head,
                    freelist_count,
                })
            },
        )
}

proptest! {
    #[test]
    fn any_valid_header_round_trips(header in any_valid_header()) {
        let mut buf = vec![0u8; header.page_size.as_usize()];
        header.encode(&mut buf)?;
        prop_assert_eq!(FileHeader::decode(&buf)?, header);
    }

    #[test]
    fn random_bytes_never_panic(buf in prop::collection::vec(any::<u8>(), 0..600)) {
        // Only checks that nothing panics; the result is ignored.
        let _ = FileHeader::decode(&buf);
    }

    #[test]
    fn random_fields_after_valid_magic_never_panic(
        mut buf in prop::collection::vec(any::<u8>(), FILE_HEADER_END..600),
        page_type in 0u8..=4,
    ) {
        // Gets past the magic and version checks so the later checks see
        // random values.
        write_bytes(&mut buf, OFF_MAGIC, &MAGIC)?;
        write_u16(&mut buf, OFF_VERSION, FORMAT_VERSION)?;
        write_u8(&mut buf, OFF_PAGE_TYPE, page_type)?;
        let _ = FileHeader::decode(&buf);
    }

    #[test]
    fn decode_accepts_only_consistent_counts(
        page_count in 0u32..10,
        head in 0u32..12,
        count in 0u32..12,
    ) {
        // Small ranges so that both valid and invalid combinations, and every
        // boundary between them, show up often.
        // The decoder accepts the fields exactly when the ADR 0003 rules hold.
        let valid = page_count >= 1
            && if head == 0 {
                count == 0
            } else {
                head < page_count && count >= 1 && count < page_count
            };
        let buf = page_with_counts(page_count, head, count)
            .map_err(|e| TestCaseError::fail(e.to_string()))?;
        let result = FileHeader::decode(&buf);
        if valid {
            prop_assert!(result.is_ok(), "({}, {}, {}) rejected: {:?}", page_count, head, count, result);
        } else {
            prop_assert!(
                matches!(result, Err(PageError::CorruptHeader { .. })),
                "({}, {}, {}) accepted or wrong error: {:?}", page_count, head, count, result
            );
        }
    }
}
