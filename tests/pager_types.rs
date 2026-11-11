//! Tests for page identifiers and page sizes (`storage::pager::types`, step 2).
//!
//! These tests check:
//!
//! - `PageSize` accepts exactly the sizes allowed by ADR 0001: powers of two
//!   in `[512, 65536]`. Any other value is rejected when it is created.
//! - `PageId::offset` gives the byte offset of a page in the file and does not
//!   overflow, even at the largest page id and page size.
//! - On disk, page id 0 means "no page", because page 0 is the file header and
//!   is never the target of a link (ADR 0003). It decodes to `None`.

use anyhow::{Context, ensure};
use proptest::prelude::*;
use storage_engine_rs::storage::pager::error::PageError;
use storage_engine_rs::storage::pager::types::{PageId, PageSize};

// ---------------------------------------------------------------------------
// PageSize
// ---------------------------------------------------------------------------

#[test]
fn page_size_accepts_powers_of_two_in_range() -> anyhow::Result<()> {
    for size in [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536] {
        let page_size = PageSize::new(size)?;
        ensure!(page_size.get() == size, "get() changed the value");
        ensure!(
            u32::try_from(page_size.as_usize())? == size,
            "as_usize() changed the value"
        );
    }
    Ok(())
}

#[test]
fn page_size_rejects_invalid_values() {
    for size in [0, 1, 256, 511, 513, 1000, 4095, 4097, 131_072, u32::MAX] {
        let result = PageSize::new(size);
        assert!(
            matches!(result, Err(PageError::InvalidPageSize { size: s }) if s == size),
            "{size} should be rejected"
        );
    }
}

#[test]
fn page_size_default_is_4_kib() {
    assert_eq!(PageSize::default().get(), 4096);
}

proptest! {
    #[test]
    fn page_size_new_accepts_exactly_valid_sizes(size: u32) {
        let valid = size.is_power_of_two() && (512..=65536).contains(&size);
        prop_assert_eq!(PageSize::new(size).is_ok(), valid);
    }
}

// ---------------------------------------------------------------------------
// PageId
// ---------------------------------------------------------------------------

#[test]
fn page_id_keeps_its_number() {
    assert_eq!(PageId::new(7).get(), 7);
    assert_eq!(PageId::HEADER.get(), 0);
}

#[test]
fn page_id_offset_in_file() -> anyhow::Result<()> {
    let page_size = PageSize::new(512)?;
    ensure!(PageId::new(0).offset(page_size) == 0, "page 0");
    ensure!(PageId::new(1).offset(page_size) == 512, "page 1");
    ensure!(PageId::new(3).offset(page_size) == 1536, "page 3");
    Ok(())
}

#[test]
fn page_id_offset_does_not_overflow_at_the_limits() -> anyhow::Result<()> {
    // The largest page id with the largest page size must still fit in a u64.
    let page_size = PageSize::new(65536)?;
    let expected = u64::from(u32::MAX)
        .checked_mul(65536)
        .context("test setup")?;
    ensure!(
        PageId::new(u32::MAX).offset(page_size) == expected,
        "wrong offset for the last page"
    );
    Ok(())
}

#[test]
fn zero_on_disk_means_no_page() {
    assert!(PageId::from_disk(0).is_none());
    assert_eq!(PageId::to_disk(None), 0);
}

#[test]
fn nonzero_on_disk_is_a_page() {
    assert!(matches!(PageId::from_disk(5), Some(id) if id.get() == 5));
    assert_eq!(PageId::to_disk(Some(PageId::new(9))), 9);
}

proptest! {
    #[test]
    fn disk_encoding_round_trip(raw: u32) {
        prop_assert_eq!(PageId::to_disk(PageId::from_disk(raw)), raw);
    }
}
