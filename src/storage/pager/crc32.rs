//! CRC-32/ISO-HDLC, the CRC used by zlib, gzip, PNG and Ethernet.
//!
//! Hand-written, as specified in ADR 0003 (`docs/decisions/0003-pager-file-format.md`):
//!
//! - reflected polynomial `0xEDB88320`
//! - initial value `0xFFFFFFFF`
//! - final XOR `0xFFFFFFFF`
//! - check value of `b"123456789"` is `0xCBF43926`
//!
//! Used to detect corrupted pages: the page checksum covers bytes `[4, page_size)`.
//!
//! The implementation is table-driven, one byte per step. The 256-entry table is
//! built at compile time by `fill_table`.
//!
//! # Future improvements
//!
//! - Slicing-by-8: use 8 tables (8 KiB) and process 8 bytes per iteration, several
//!   times faster. Worth it when the WAL checksums large volumes. See Database
//!   Internals ch. 3, "Checksumming".
//! - CRC32C (Castagnoli, polynomial `0x82F63B78`): has hardware instructions (SSE4.2
//!   `crc32`, the ARM v8 CRC extension), and is used by `RocksDB`, `LevelDB` and ext4. Switching changes
//!   the on-disk format, so it requires a new ADR superseding the checksum part of
//!   ADR 0003 and bumping the file format version.

// Reflected (bit-reversed) generator polynomial; 0x04C11DB7 in normal form.
const POLYNOMIAL: u32 = 0xEDB8_8320;
// Initial register value before any byte is processed.
const INIT: u32 = 0xFFFF_FFFF;
// Final XOR applied to the register; equal to INIT here by coincidence, but a separate step.
const XOR_OUT: u32 = 0xFFFF_FFFF;

/// Builds the CRC lookup table at compile time.
///
/// Entry `n` is the CRC of the single byte `n`, computed bit by bit. The loop walks
/// the array with slice patterns instead of indexing, because of the
/// `indexing_slicing` lint.
const fn fill_table() -> [u32; 256] {
    let mut arr = [0u32; 256];
    let mut n = 0u32;
    let mut rest: &mut [u32] = &mut arr;
    while let [slot, tail @ ..] = rest {
        let mut crc = n;

        let mut bit = 0u8;
        while bit < 8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ POLYNOMIAL;
            } else {
                crc >>= 1;
            }

            bit = bit.wrapping_add(1);
        }

        *slot = crc;
        rest = tail;
        n = n.wrapping_add(1);
    }
    arr
}

/// Precomputed CRC of every possible byte value, built by `fill_table`.
static LOOKUP_TABLE: [u32; 256] = fill_table();

/// Computes the CRC-32 of `input` in one call.
///
/// # Examples
///
/// ```
/// use storage_engine_rs::storage::pager::crc32::crc32;
///
/// assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
/// ```
#[must_use]
pub fn crc32(input: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(input);
    crc.finalize()
}

/// Incremental CRC-32 hasher.
///
/// Use it for data that is not contiguous in memory, such as a WAL record header
/// followed by its payload. Feeding the pieces through [`Crc32::update`] gives the
/// same result as one [`crc32`] call over their concatenation.
///
/// # Examples
///
/// ```
/// use storage_engine_rs::storage::pager::crc32::Crc32;
///
/// let mut crc = Crc32::new();
/// crc.update(b"1234");
/// crc.update(b"56789");
/// assert_eq!(crc.finalize(), 0xCBF4_3926);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Crc32 {
    crc: u32,
}

impl Crc32 {
    /// Creates a hasher in the initial state, with no bytes fed yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { crc: INIT }
    }

    /// Feeds `input` into the running checksum. Can be called any number of times.
    // TODO(perf): slicing-by-8, see module docs.
    pub fn update(&mut self, input: &[u8]) {
        for val in input {
            // Takes the low byte without an `as` cast.
            let [low, ..] = self.crc.to_le_bytes();
            self.crc = LOOKUP_TABLE
                .get(usize::from(low ^ val))
                .copied()
                // The index is `low ^ val`, both u8, so it is always < 256 and this
                // fallback is unreachable. `get` is used only to satisfy the
                // indexing_slicing lint.
                .unwrap_or(0)
                ^ (self.crc >> 8);
        }
    }

    /// Applies the final XOR and returns the CRC-32 of all bytes fed so far.
    ///
    /// Consumes the hasher.
    #[must_use]
    pub const fn finalize(self) -> u32 {
        self.crc ^ XOR_OUT
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::LOOKUP_TABLE;

    // Checks the table directly so a failure points at fill_table, not at a final checksum.
    #[test]
    fn lookup_table_has_known_entries() {
        assert_eq!(LOOKUP_TABLE.len(), 256);
        assert_eq!(LOOKUP_TABLE.first(), Some(&0x0000_0000));
        assert_eq!(LOOKUP_TABLE.get(1), Some(&0x7707_3096));
        assert_eq!(LOOKUP_TABLE.get(128), Some(&0xEDB8_8320));
        assert_eq!(LOOKUP_TABLE.get(255), Some(&0x2D02_EF8D));
    }
}
