//! Tests for the common page header (`storage::pager::page`, step 4).
//!
//! The byte layout is documented in `src/storage/pager/page.rs` and in
//! ADR 0003. These tests check the behavior the rest of the pager relies on:
//!
//! - `PageType` round-trips through `u8`. Only 1, 2 and 3 are valid, so a
//!   zero-filled page is never taken for a real one.
//! - Each setter writes only its own field, in little-endian.
//! - The checksum is the CRC32 of `[4, len)`, stored at `[0, 4)`. Any changed
//!   byte, including a checksum byte, fails verification, and so does any
//!   change made after stamping.
//! - Errors carry the values needed to debug them (`stored` and `computed`,
//!   `expected` and `found`).
//! - A buffer shorter than the 16-byte header is an error in every function,
//!   never a panic, and a failed setter leaves the buffer unchanged.
//!
//! Expected values do not come from the code under test: offsets are written
//! as literals, and checksums come from `crc32` (step 3) or from Python's
//! `zlib.crc32`.

use anyhow::{Context, ensure};
use proptest::prelude::*;
use storage_engine_rs::storage::pager::crc32::crc32;
use storage_engine_rs::storage::pager::error::PageError;
use storage_engine_rs::storage::pager::page::{
    COMMON_HEADER_SIZE, PageType, compute_checksum, expect_page_type, page_lsn, page_type,
    set_page_lsn, set_page_type, stamp_checksum, verify_checksum,
};

/// Page sizes allowed by ADR 0001 that the tests cover: smallest, default, largest.
const SIZES: [usize; 3] = [512, 4096, 65536];

/// Builds a page of `size` bytes with a valid header: type Slotted, LSN 42,
/// a recognizable body, and a stamped checksum.
fn valid_page(size: usize) -> anyhow::Result<Vec<u8>> {
    let mut buf = unstamped_page(size)?;
    stamp_checksum(&mut buf)?;
    Ok(buf)
}

/// Same as `valid_page`, but the checksum field is left as zeros.
fn unstamped_page(size: usize) -> anyhow::Result<Vec<u8>> {
    let mut buf: Vec<u8> = (0..size)
        .map(|i| {
            let [low, ..] = i.to_le_bytes();
            low
        })
        .collect();
    buf.get_mut(0..4).context("bytes 0..4")?.fill(0);
    set_page_type(&mut buf, PageType::Slotted)?;
    set_page_lsn(&mut buf, 42)?;
    Ok(buf)
}

/// Reads the stored checksum straight from `[0, 4)`, without the API under test.
fn stored_checksum(buf: &[u8]) -> anyhow::Result<u32> {
    let bytes = buf.get(0..4).context("buffer shorter than 4 bytes")?;
    Ok(u32::from_le_bytes(bytes.try_into()?))
}

/// CRC32 of `[4, len)` computed with the step 3 function, as an independent reference.
fn expected_checksum(buf: &[u8]) -> anyhow::Result<u32> {
    Ok(crc32(buf.get(4..).context("buffer shorter than 4 bytes")?))
}

// ---------------------------------------------------------------------------
// Constants and PageType conversions
// ---------------------------------------------------------------------------

#[test]
fn common_header_is_16_bytes() {
    assert_eq!(COMMON_HEADER_SIZE, 16);
}

#[test]
fn page_type_to_byte_matches_adr() {
    assert_eq!(u8::from(PageType::FileHeader), 1);
    assert_eq!(u8::from(PageType::Free), 2);
    assert_eq!(u8::from(PageType::Slotted), 3);
}

#[test]
fn page_type_from_byte_round_trips() -> anyhow::Result<()> {
    for page_type in [PageType::FileHeader, PageType::Free, PageType::Slotted] {
        let back = PageType::try_from(u8::from(page_type))?;
        ensure!(back == page_type, "{page_type:?} came back as {back:?}");
    }
    Ok(())
}

#[test]
fn page_type_rejects_every_unknown_byte() {
    // Checks all 256 values: only 1, 2 and 3 are valid.
    for value in 0..=u8::MAX {
        let result = PageType::try_from(value);
        if (1..=3).contains(&value) {
            assert!(result.is_ok(), "{value} should be accepted");
        } else {
            assert!(
                matches!(result, Err(PageError::UnknownPageType { value: v }) if v == value),
                "{value} should be rejected with UnknownPageType, got {result:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Page type field (offset 4)
// ---------------------------------------------------------------------------

#[test]
fn set_page_type_writes_byte_4() -> anyhow::Result<()> {
    let mut buf = vec![0u8; 512];
    set_page_type(&mut buf, PageType::Free)?;
    ensure!(buf.get(4) == Some(&2), "byte 4 is {:?}", buf.get(4));
    ensure!(
        page_type(&buf)? == PageType::Free,
        "page_type did not read it back"
    );
    Ok(())
}

#[test]
fn set_page_type_touches_only_byte_4() -> anyhow::Result<()> {
    let mut buf = vec![0xAAu8; 512];
    set_page_type(&mut buf, PageType::Slotted)?;
    for (i, byte) in buf.iter().enumerate() {
        let expected = if i == 4 { 3 } else { 0xAA };
        ensure!(
            *byte == expected,
            "byte {i} is {byte:#04x}, expected {expected:#04x}"
        );
    }
    Ok(())
}

#[test]
fn page_type_of_zeroed_page_is_unknown() {
    // A page that was allocated but never written is all zeros.
    let buf = vec![0u8; 512];
    let result = page_type(&buf);
    assert!(
        matches!(result, Err(PageError::UnknownPageType { value: 0 })),
        "got {result:?}"
    );
}

#[test]
fn page_type_rejects_unknown_byte_in_buffer() -> anyhow::Result<()> {
    let mut buf = vec![0u8; 512];
    *buf.get_mut(4).context("byte 4")? = 0xFF;
    let result = page_type(&buf);
    ensure!(
        matches!(result, Err(PageError::UnknownPageType { value: 0xFF })),
        "got {result:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Page LSN field (offset 8)
// ---------------------------------------------------------------------------

#[test]
fn set_page_lsn_is_little_endian_at_offset_8() -> anyhow::Result<()> {
    let mut buf = vec![0u8; 512];
    set_page_lsn(&mut buf, 0x0102_0304_0506_0708)?;
    let field = buf.get(8..16).context("bytes 8..16")?;
    ensure!(
        field == [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
        "got {field:02x?}"
    );
    Ok(())
}

#[test]
fn set_page_lsn_touches_only_bytes_8_to_16() -> anyhow::Result<()> {
    let mut buf = vec![0xAAu8; 512];
    set_page_lsn(&mut buf, 0)?;
    for (i, byte) in buf.iter().enumerate() {
        let expected = if (8..16).contains(&i) { 0 } else { 0xAA };
        ensure!(
            *byte == expected,
            "byte {i} is {byte:#04x}, expected {expected:#04x}"
        );
    }
    Ok(())
}

#[test]
fn page_lsn_holds_extreme_values() -> anyhow::Result<()> {
    let mut buf = vec![0u8; 512];
    for lsn in [0, 1, u64::MAX] {
        set_page_lsn(&mut buf, lsn)?;
        ensure!(page_lsn(&buf)? == lsn, "LSN {lsn} did not round-trip");
    }
    Ok(())
}

#[test]
fn page_type_and_lsn_are_independent() -> anyhow::Result<()> {
    let mut buf = vec![0u8; 512];
    set_page_lsn(&mut buf, u64::MAX)?;
    set_page_type(&mut buf, PageType::FileHeader)?;
    ensure!(page_lsn(&buf)? == u64::MAX, "set_page_type changed the LSN");
    set_page_lsn(&mut buf, 7)?;
    ensure!(
        page_type(&buf)? == PageType::FileHeader,
        "set_page_lsn changed the page type"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Checksum (offset 0, covers [4, len))
// ---------------------------------------------------------------------------

#[test]
fn stamp_then_verify_passes_for_every_size() -> anyhow::Result<()> {
    for size in SIZES {
        let buf = valid_page(size)?;
        verify_checksum(&buf).with_context(|| format!("size {size}"))?;
    }
    Ok(())
}

#[test]
fn stamp_writes_crc_of_bytes_4_to_end_in_little_endian() -> anyhow::Result<()> {
    for size in SIZES {
        let buf = valid_page(size)?;
        let stored = stored_checksum(&buf)?;
        let expected = expected_checksum(&buf)?;
        ensure!(
            stored == expected,
            "size {size}: stored {stored:#010x}, expected {expected:#010x}"
        );
    }
    Ok(())
}

#[test]
fn compute_checksum_matches_reference() -> anyhow::Result<()> {
    for size in SIZES {
        let buf = valid_page(size)?;
        let computed = compute_checksum(&buf)?;
        let expected = expected_checksum(&buf)?;
        ensure!(
            computed == expected,
            "size {size}: computed {computed:#010x}, expected {expected:#010x}"
        );
    }
    Ok(())
}

#[test]
fn compute_checksum_ignores_stored_checksum() -> anyhow::Result<()> {
    // The checksum field is not part of the bytes it protects.
    let mut buf = valid_page(512)?;
    let before = compute_checksum(&buf)?;
    buf.get_mut(0..4)
        .context("bytes 0..4")?
        .copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let after = compute_checksum(&buf)?;
    ensure!(before == after, "{before:#010x} != {after:#010x}");
    Ok(())
}

#[test]
fn stamp_overwrites_previous_checksum() -> anyhow::Result<()> {
    let mut buf = valid_page(512)?;
    buf.get_mut(0..4)
        .context("bytes 0..4")?
        .copy_from_slice(&[0xFF; 4]);
    stamp_checksum(&mut buf)?;
    verify_checksum(&buf)?;
    Ok(())
}

#[test]
fn stamp_touches_only_bytes_0_to_4() -> anyhow::Result<()> {
    let original = unstamped_page(512)?;
    let mut buf = original.clone();
    stamp_checksum(&mut buf)?;
    ensure!(
        stored_checksum(&buf)? == expected_checksum(&buf)?,
        "stamp_checksum did not write the checksum"
    );
    ensure!(
        buf.get(4..) == original.get(4..),
        "stamp_checksum changed bytes outside [0, 4)"
    );
    Ok(())
}

#[test]
fn mismatch_reports_stored_and_computed() -> anyhow::Result<()> {
    let mut buf = valid_page(512)?;
    let stored = stored_checksum(&buf)?;
    *buf.get_mut(100).context("byte 100")? ^= 0x01;
    let computed = expected_checksum(&buf)?;

    let result = verify_checksum(&buf);
    ensure!(
        result == Err(PageError::ChecksumMismatch { stored, computed }),
        "expected stored {stored:#010x} and computed {computed:#010x}, got {result:?}"
    );
    Ok(())
}

#[test]
fn corrupted_checksum_field_fails() -> anyhow::Result<()> {
    let mut buf = valid_page(512)?;
    *buf.get_mut(0).context("byte 0")? ^= 0x80;
    let result = verify_checksum(&buf);
    ensure!(
        matches!(result, Err(PageError::ChecksumMismatch { .. })),
        "got {result:?}"
    );
    Ok(())
}

#[test]
fn zeroed_page_fails_verification() {
    // A zero-filled page never passes: the CRC of zeros is not zero.
    // Values computed with Python's zlib.crc32(bytes(size - 4)).
    for (size, computed) in [
        (512, 0x0FBB_6D54),
        (4096, 0x603B_0489),
        (65536, 0x510B_66BF),
    ] {
        let buf = vec![0u8; size];
        let result = verify_checksum(&buf);
        assert_eq!(
            result,
            Err(PageError::ChecksumMismatch {
                stored: 0,
                computed
            }),
            "size {size}"
        );
    }
}

#[test]
fn changing_lsn_after_stamp_invalidates_checksum() -> anyhow::Result<()> {
    // The pager must stamp the checksum last, after every other change.
    let mut buf = valid_page(512)?;
    set_page_lsn(&mut buf, 43)?;
    ensure!(
        matches!(
            verify_checksum(&buf),
            Err(PageError::ChecksumMismatch { .. })
        ),
        "a changed LSN must fail verification until the page is stamped again"
    );
    stamp_checksum(&mut buf)?;
    verify_checksum(&buf)?;
    Ok(())
}

#[test]
fn changing_type_after_stamp_invalidates_checksum() -> anyhow::Result<()> {
    let mut buf = valid_page(512)?;
    set_page_type(&mut buf, PageType::Free)?;
    ensure!(
        matches!(
            verify_checksum(&buf),
            Err(PageError::ChecksumMismatch { .. })
        ),
        "a changed page type must fail verification"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// expect_page_type
// ---------------------------------------------------------------------------

#[test]
fn expect_page_type_accepts_matching_type() -> anyhow::Result<()> {
    let buf = valid_page(512)?;
    expect_page_type(&buf, PageType::Slotted)?;
    Ok(())
}

#[test]
fn expect_page_type_reports_expected_and_found() -> anyhow::Result<()> {
    let buf = valid_page(512)?;
    let result = expect_page_type(&buf, PageType::Free);
    ensure!(
        result
            == Err(PageError::UnexpectedPageType {
                expected: PageType::Free,
                found: PageType::Slotted,
            }),
        "got {result:?}"
    );
    Ok(())
}

#[test]
fn expect_page_type_on_zeroed_page_is_unknown() {
    let buf = vec![0u8; 512];
    let result = expect_page_type(&buf, PageType::Free);
    assert!(
        matches!(result, Err(PageError::UnknownPageType { value: 0 })),
        "got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Short buffers: every function errors, nothing panics, nothing is written
// ---------------------------------------------------------------------------

#[test]
fn readers_reject_buffers_shorter_than_header() {
    for len in 0..COMMON_HEADER_SIZE {
        let buf = vec![0u8; len];
        assert!(
            matches!(page_type(&buf), Err(PageError::OutOfBounds { .. })),
            "page_type, len {len}"
        );
        assert!(
            matches!(page_lsn(&buf), Err(PageError::OutOfBounds { .. })),
            "page_lsn, len {len}"
        );
        assert!(
            matches!(compute_checksum(&buf), Err(PageError::OutOfBounds { .. })),
            "compute_checksum, len {len}"
        );
        assert!(
            matches!(verify_checksum(&buf), Err(PageError::OutOfBounds { .. })),
            "verify_checksum, len {len}"
        );
        assert!(
            matches!(
                expect_page_type(&buf, PageType::Free),
                Err(PageError::OutOfBounds { .. })
            ),
            "expect_page_type, len {len}"
        );
    }
}

#[test]
fn writers_reject_short_buffers_without_writing() {
    // Byte 4 fits in a 5-byte buffer, but a page needs the whole header.
    // A setter must check the header size, not only its own field.
    for len in 0..COMMON_HEADER_SIZE {
        let mut buf = vec![0xAAu8; len];
        assert!(
            matches!(
                set_page_type(&mut buf, PageType::Free),
                Err(PageError::OutOfBounds { .. })
            ),
            "set_page_type, len {len}"
        );
        assert!(
            matches!(
                set_page_lsn(&mut buf, 1),
                Err(PageError::OutOfBounds { .. })
            ),
            "set_page_lsn, len {len}"
        );
        assert!(
            matches!(stamp_checksum(&mut buf), Err(PageError::OutOfBounds { .. })),
            "stamp_checksum, len {len}"
        );
        assert!(
            buf.iter().all(|b| *b == 0xAA),
            "a failed call modified a {len}-byte buffer"
        );
    }
}

#[test]
fn exactly_header_sized_buffer_works() -> anyhow::Result<()> {
    // 16 bytes is the smallest valid input. The checksum then covers [4, 16).
    let mut buf = vec![0u8; COMMON_HEADER_SIZE];
    set_page_type(&mut buf, PageType::Free)?;
    set_page_lsn(&mut buf, 9)?;
    stamp_checksum(&mut buf)?;
    verify_checksum(&buf)?;
    ensure!(
        stored_checksum(&buf)? == expected_checksum(&buf)?,
        "checksum of a 16-byte buffer is wrong"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn lsn_round_trips(lsn: u64) {
        let mut buf = vec![0u8; 512];
        set_page_lsn(&mut buf, lsn)?;
        prop_assert_eq!(page_lsn(&buf)?, lsn);
    }

    #[test]
    fn any_single_byte_change_is_detected(
        size in prop::sample::select(SIZES.to_vec()),
        pos_seed: usize,
        mask in 1u8..=255,
    ) {
        // CRC32 detects every error burst of 32 bits or less, so changing any
        // one byte, including a checksum byte, must always fail verification.
        let mut buf = valid_page(size).map_err(|e| TestCaseError::fail(e.to_string()))?;
        let pos = pos_seed.checked_rem(size).unwrap_or(0);
        if let Some(byte) = buf.get_mut(pos) {
            *byte ^= mask;
        }
        let result = verify_checksum(&buf);
        prop_assert!(
            matches!(result, Err(PageError::ChecksumMismatch { .. })),
            "size {}, byte {} xor {:#04x}: got {:?}", size, pos, mask, result
        );
    }

    #[test]
    fn random_bytes_never_panic(mut buf in prop::collection::vec(any::<u8>(), 0..600)) {
        // Only checks that nothing panics; the results are ignored.
        let _ = page_type(&buf);
        let _ = page_lsn(&buf);
        let _ = compute_checksum(&buf);
        let _ = verify_checksum(&buf);
        let _ = expect_page_type(&buf, PageType::Slotted);
        let _ = set_page_type(&mut buf, PageType::Free);
        let _ = set_page_lsn(&mut buf, u64::MAX);
        let _ = stamp_checksum(&mut buf);
    }

    #[test]
    fn stamp_then_verify_on_random_pages(
        mut buf in prop::collection::vec(any::<u8>(), COMMON_HEADER_SIZE..2048),
        lsn: u64,
    ) {
        set_page_type(&mut buf, PageType::Free)?;
        set_page_lsn(&mut buf, lsn)?;
        stamp_checksum(&mut buf)?;
        prop_assert!(verify_checksum(&buf).is_ok());
        prop_assert_eq!(page_type(&buf)?, PageType::Free);
        prop_assert_eq!(page_lsn(&buf)?, lsn);
    }
}
