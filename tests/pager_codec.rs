//! Step 2 (TDD): little-endian byte codec for page buffers.
//!
//! Every function takes a buffer and an offset, and must return an error
//! instead of panicking when the access does not fit in the buffer.

use anyhow::{Context, ensure};
use proptest::prelude::*;
use storage_engine_rs::storage::pager::codec::{
    read_u8, read_u16, read_u32, read_u64, slice, write_bytes, write_u8, write_u16, write_u32,
    write_u64,
};
use storage_engine_rs::storage::pager::error::PageError;

// ---------------------------------------------------------------------------
// Byte order
// ---------------------------------------------------------------------------

#[test]
fn write_u16_is_little_endian() -> anyhow::Result<()> {
    let mut buf = [0u8; 2];
    write_u16(&mut buf, 0, 0x0102)?;
    ensure!(buf == [0x02, 0x01], "got {buf:02x?}");
    Ok(())
}

#[test]
fn write_u32_is_little_endian() -> anyhow::Result<()> {
    let mut buf = [0u8; 4];
    write_u32(&mut buf, 0, 0x0102_0304)?;
    ensure!(buf == [0x04, 0x03, 0x02, 0x01], "got {buf:02x?}");
    Ok(())
}

#[test]
fn write_u64_is_little_endian() -> anyhow::Result<()> {
    let mut buf = [0u8; 8];
    write_u64(&mut buf, 0, 0x0102_0304_0506_0708)?;
    ensure!(
        buf == [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
        "got {buf:02x?}"
    );
    Ok(())
}

#[test]
fn read_u32_decodes_little_endian() -> anyhow::Result<()> {
    let buf = [0x78, 0x56, 0x34, 0x12];
    let value = read_u32(&buf, 0)?;
    ensure!(value == 0x1234_5678, "got {value:#x}");
    Ok(())
}

#[test]
fn read_u8_and_write_u8() -> anyhow::Result<()> {
    let mut buf = [0u8; 3];
    write_u8(&mut buf, 1, 0xAB)?;
    ensure!(buf == [0x00, 0xAB, 0x00], "got {buf:02x?}");
    ensure!(
        read_u8(&buf, 1)? == 0xAB,
        "read_u8 returned the wrong value {:#x}",
        read_u8(&buf, 1)?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Offsets
// ---------------------------------------------------------------------------

#[test]
fn write_at_offset_touches_only_its_bytes() -> anyhow::Result<()> {
    let mut buf = [0u8; 8];
    write_u16(&mut buf, 3, 0xABCD)?;
    ensure!(
        buf == [0x00, 0x00, 0x00, 0xCD, 0xAB, 0x00, 0x00, 0x00],
        "got {buf:02x?}"
    );
    Ok(())
}

#[test]
fn last_valid_offset_works() -> anyhow::Result<()> {
    let mut buf = [0u8; 10];
    // A u32 at offset 6 uses bytes 6, 7, 8, 9: the last four bytes.
    write_u32(&mut buf, 6, 0xDEAD_BEEF)?;
    ensure!(
        read_u32(&buf, 6)? == 0xDEAD_BEEF,
        "round trip at the end failed"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Out of bounds: errors, never panics
// ---------------------------------------------------------------------------

#[test]
fn read_past_the_end_is_out_of_bounds() {
    let buf = [0u8; 10];
    // A u32 at offset 7 would need bytes 7..11, but the buffer ends at 10.
    let result = read_u32(&buf, 7);
    assert!(matches!(
        result,
        Err(PageError::OutOfBounds {
            offset: 7,
            len: 4,
            buf_len: 10
        })
    ));
}

#[test]
fn write_past_the_end_is_out_of_bounds_and_leaves_buffer_unchanged() {
    let mut buf = [0xAAu8; 10];
    let result = write_u32(&mut buf, 7, 0x0102_0304);
    assert!(matches!(
        result,
        Err(PageError::OutOfBounds {
            offset: 7,
            len: 4,
            buf_len: 10
        })
    ));
    // A failed write must not write anything, not even the bytes that fit.
    assert!(buf == [0xAAu8; 10]);
}

#[test]
fn offset_equal_to_len_is_out_of_bounds() {
    let buf = [0u8; 4];
    assert!(matches!(
        read_u8(&buf, 4),
        Err(PageError::OutOfBounds { .. })
    ));
}

#[test]
fn empty_buffer_is_out_of_bounds() {
    let buf: [u8; 0] = [];
    assert!(matches!(
        read_u8(&buf, 0),
        Err(PageError::OutOfBounds { .. })
    ));
    assert!(matches!(
        read_u64(&buf, 0),
        Err(PageError::OutOfBounds { .. })
    ));
}

#[test]
fn huge_offset_does_not_overflow() {
    // `usize::MAX + 4` overflows. This must be an error, not a panic.
    let mut buf = [0u8; 16];
    assert!(matches!(
        read_u32(&buf, usize::MAX),
        Err(PageError::OutOfBounds { .. })
    ));
    assert!(matches!(
        write_u64(&mut buf, usize::MAX, 1),
        Err(PageError::OutOfBounds { .. })
    ));
    assert!(matches!(
        slice(&buf, 1, usize::MAX),
        Err(PageError::OutOfBounds { .. })
    ));
}

// ---------------------------------------------------------------------------
// Raw bytes: write_bytes and slice
// ---------------------------------------------------------------------------

#[test]
fn write_bytes_copies_at_offset() -> anyhow::Result<()> {
    let mut buf = [0u8; 8];
    write_bytes(&mut buf, 2, b"abc")?;
    ensure!(buf == *b"\0\0abc\0\0\0", "got {buf:02x?}");
    Ok(())
}

#[test]
fn write_bytes_past_the_end_is_out_of_bounds_and_leaves_buffer_unchanged() {
    let mut buf = [0u8; 4];
    let result = write_bytes(&mut buf, 2, b"abc");
    assert!(matches!(
        result,
        Err(PageError::OutOfBounds {
            offset: 2,
            len: 3,
            buf_len: 4
        })
    ));
    assert!(buf == [0u8; 4]);
}

#[test]
fn empty_range_at_the_end_is_valid() -> anyhow::Result<()> {
    // Like Rust slices: `buf[len..len]` is a valid empty range.
    let mut buf = [0u8; 4];
    write_bytes(&mut buf, 4, &[])?;
    ensure!(slice(&buf, 4, 0)?.is_empty(), "expected an empty slice");
    Ok(())
}

#[test]
fn slice_returns_the_requested_bytes() -> anyhow::Result<()> {
    let buf = *b"hello world";
    ensure!(slice(&buf, 6, 5)? == b"world", "wrong bytes");
    Ok(())
}

#[test]
fn slice_borrows_from_the_buffer_without_copying() -> anyhow::Result<()> {
    let buf = *b"hello world";
    let got = slice(&buf, 6, 5)?;
    let expected = buf.get(6..11).context("test setup")?;
    // Same address and same length: `slice` must be a view, not a copy.
    ensure!(std::ptr::eq(got, expected), "slice copied the data");
    Ok(())
}

#[test]
fn slice_past_the_end_is_out_of_bounds() {
    let buf = [0u8; 4];
    assert!(matches!(
        slice(&buf, 3, 2),
        Err(PageError::OutOfBounds {
            offset: 3,
            len: 2,
            buf_len: 4
        })
    ));
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

const BUF_LEN: usize = 64;

proptest! {
    #[test]
    fn u16_round_trip(value: u16, offset in 0..=BUF_LEN - 2) {
        let mut buf = [0u8; BUF_LEN];
        write_u16(&mut buf, offset, value)?;
        prop_assert_eq!(read_u16(&buf, offset)?, value);
    }

    #[test]
    fn u32_round_trip(value: u32, offset in 0..=BUF_LEN - 4) {
        let mut buf = [0u8; BUF_LEN];
        write_u32(&mut buf, offset, value)?;
        prop_assert_eq!(read_u32(&buf, offset)?, value);
    }

    #[test]
    fn u64_round_trip(value: u64, offset in 0..=BUF_LEN - 8) {
        let mut buf = [0u8; BUF_LEN];
        write_u64(&mut buf, offset, value)?;
        prop_assert_eq!(read_u64(&buf, offset)?, value);
    }

    #[test]
    fn write_never_touches_other_bytes(value: u64, offset in 0..=BUF_LEN - 8) {
        let mut buf = [0x5Au8; BUF_LEN];
        write_u64(&mut buf, offset, value)?;
        for (i, byte) in buf.iter().enumerate() {
            // Bytes offset..offset + 8 belong to the value; the rest must not change.
            let inside = i.checked_sub(offset).is_some_and(|d| d < 8);
            prop_assert!(inside || *byte == 0x5A, "byte {} changed", i);
        }
    }

    #[test]
    fn any_offset_never_panics(offset: usize) {
        let mut buf = [0u8; BUF_LEN];
        let fits = offset <= BUF_LEN - 8;
        prop_assert_eq!(read_u64(&buf, offset).is_ok(), fits);
        prop_assert_eq!(write_u64(&mut buf, offset, 7).is_ok(), fits);
    }
}
