//! Tests for the hand-written CRC32 (`storage::pager::crc32`, step 3).
//!
//! The variant is CRC-32/ISO-HDLC, the one used by zlib, gzip, PNG and
//! Ethernet (ADR 0003). These tests check:
//!
//! - Known vectors, including the standard check value of `"123456789"`.
//!   If it matches, the polynomial, initial value and final XOR are right.
//! - The incremental `Crc32` gives the same result as one `crc32` call, however
//!   the input is split.
//! - Flipping any single bit changes the checksum.
//!
//! Every expected value was computed with Python's `zlib.crc32`, an
//! independent implementation.

use anyhow::ensure;
use proptest::prelude::*;
use storage_engine_rs::storage::pager::crc32::{Crc32, crc32};

/// Checks `crc32(input) == expected` with a readable error message.
fn check(input: &[u8], expected: u32) -> anyhow::Result<()> {
    let got = crc32(input);
    ensure!(
        got == expected,
        "crc32 of {} bytes: expected {expected:#010x}, got {got:#010x}",
        input.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Known vectors
// ---------------------------------------------------------------------------

#[test]
fn empty_input_is_zero() -> anyhow::Result<()> {
    // Initial value 0xFFFFFFFF XOR final value 0xFFFFFFFF = 0.
    check(b"", 0x0000_0000)
}

#[test]
fn single_byte() -> anyhow::Result<()> {
    check(b"a", 0xE8B7_BE43)
}

#[test]
fn short_string() -> anyhow::Result<()> {
    check(b"abc", 0x3524_41C2)
}

#[test]
fn standard_check_value() -> anyhow::Result<()> {
    // "123456789" is the standard check input for every CRC variant.
    // If this passes, the parameters (polynomial, init, xor) are right.
    check(b"123456789", 0xCBF4_3926)
}

#[test]
fn sentence() -> anyhow::Result<()> {
    check(b"The quick brown fox jumps over the lazy dog", 0x414F_A339)
}

#[test]
fn all_zero_bytes() -> anyhow::Result<()> {
    check(&[0u8; 16], 0xECBB_4B55)
}

#[test]
fn all_one_bits() -> anyhow::Result<()> {
    check(&[0xFFu8; 16], 0x3FB3_C61A)
}

#[test]
fn zeroed_page_body_is_not_zero() -> anyhow::Result<()> {
    // A freshly allocated 512-byte page is all zeros. Its checksum covers
    // bytes [4, 512): 508 zeros. The CRC is not 0, so a page that was never
    // written fails verification (plan decision D12).
    check(&[0u8; 508], 0x0FBB_6D54)
}

#[test]
fn full_4_kib_page() -> anyhow::Result<()> {
    // Bytes 0, 1, 2, ..., 255, 0, 1, ... over a 4096-byte page.
    let page: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    check(&page, 0xA291_2082)
}

// ---------------------------------------------------------------------------
// Incremental API
// ---------------------------------------------------------------------------

#[test]
fn new_then_finalize_is_crc_of_empty_input() {
    assert_eq!(Crc32::new().finalize(), 0);
}

#[test]
fn incremental_in_two_parts_matches_one_shot() -> anyhow::Result<()> {
    let mut hasher = Crc32::new();
    hasher.update(b"12345");
    hasher.update(b"6789");
    let got = hasher.finalize();
    ensure!(got == 0xCBF4_3926, "got {got:#010x}");
    Ok(())
}

#[test]
fn incremental_byte_by_byte_matches_one_shot() -> anyhow::Result<()> {
    let input = b"The quick brown fox jumps over the lazy dog";
    let mut hasher = Crc32::new();
    for byte in input {
        hasher.update(std::slice::from_ref(byte));
    }
    let got = hasher.finalize();
    ensure!(got == crc32(input), "got {got:#010x}");
    Ok(())
}

#[test]
fn empty_updates_change_nothing() -> anyhow::Result<()> {
    let mut hasher = Crc32::new();
    hasher.update(b"");
    hasher.update(b"123456789");
    hasher.update(b"");
    let got = hasher.finalize();
    ensure!(got == 0xCBF4_3926, "got {got:#010x}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn split_anywhere_matches_one_shot(
        data in proptest::collection::vec(any::<u8>(), 0..512),
        split in any::<prop::sample::Index>(),
    ) {
        // Cut the input at any point (0..=len) and feed both halves.
        let at = split.index(data.len().saturating_add(1));
        let (head, tail) = data.split_at_checked(at).unwrap_or((&data, &[]));
        let mut hasher = Crc32::new();
        hasher.update(head);
        hasher.update(tail);
        prop_assert_eq!(hasher.finalize(), crc32(&data));
    }

    #[test]
    fn any_single_bit_flip_changes_the_checksum(
        data in proptest::collection::vec(any::<u8>(), 1..512),
        position in any::<prop::sample::Index>(),
        bit in 0u32..8,
    ) {
        // CRC32 detects every single-bit error. This is why it protects
        // pages against corruption on disk.
        let original = crc32(&data);
        let mut corrupted = data;
        let index = position.index(corrupted.len());
        if let Some(byte) = corrupted.get_mut(index) {
            *byte ^= 1u8.wrapping_shl(bit);
        }
        prop_assert_ne!(crc32(&corrupted), original);
    }

    #[test]
    fn checksum_is_deterministic(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        prop_assert_eq!(crc32(&data), crc32(&data));
    }
}
