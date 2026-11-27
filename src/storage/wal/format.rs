//! WAL file format: header, records and the log reader (ADR 0006).
//!
//! Header (32 bytes at offset 0, little-endian):
//!
//! | Offset | Size | Field                                   |
//! |--------|------|-----------------------------------------|
//! | 0      | 8    | magic: `STENGWAL`                       |
//! | 8      | 2    | version: u16, 1                         |
//! | 10     | 2    | reserved                                |
//! | 12     | 4    | `page_size`: u32                        |
//! | 16     | 8    | `base_lsn`: u64, LSN of byte 0          |
//! | 24     | 4    | reserved                                |
//! | 28     | 4    | crc32 of bytes `[0, 28)`                |
//!
//! Record (one per operation):
//!
//! | Offset | Size                | Field                                       |
//! |--------|---------------------|---------------------------------------------|
//! | 0      | 4                   | crc32 of bytes `[4, total_len)`             |
//! | 4      | 4                   | `total_len`: `24 + n * (4 + page_size)`     |
//! | 8      | 8                   | lsn: `base_lsn + offset of the record`      |
//! | 16     | 1                   | kind: 1 = page images                       |
//! | 17     | 3                   | reserved                                    |
//! | 20     | 4                   | n: number of images, >= 1                   |
//! | 24     | n * (4 + page_size) | `(page_id: u32, image: [u8; page_size])`    |

use crate::storage::{
    pager::{
        codec::{
            read_u8, read_u16, read_u32, read_u64, slice, write_bytes, write_u8, write_u16,
            write_u32, write_u64,
        },
        crc32::crc32,
        page::{page_lsn, verify_checksum},
        types::{PageId, PageSize},
    },
    wal::error::WalError,
};

/// Identifies a WAL file: ASCII `STENGWAL`.
pub const MAGIC: [u8; 8] = *b"STENGWAL";
/// WAL format version written by this build.
pub const VERSION: u16 = 1;
/// Size of the WAL header in bytes. Also the smallest possible LSN.
pub const HEADER_SIZE: usize = 32;
/// Size of the fixed part of a record, before the images.
pub const RECORD_HEADER_SIZE: usize = 24;
/// Record kind: a set of full page images.
pub const KIND_PAGE_IMAGES: u8 = 1;

const OFF_VERSION: usize = 8;
const OFF_PAGE_SIZE: usize = 12;
const OFF_BASE_LSN: usize = 16;
const OFF_HEADER_CRC: usize = 28;

const OFF_REC_LEN: usize = 4;
const OFF_REC_LSN: usize = 8;
const OFF_REC_KIND: usize = 16;
const OFF_REC_COUNT: usize = 20;

/// Contents of the WAL header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalHeader {
    /// Page size of every image in the log. Must equal the database page size.
    pub page_size: PageSize,
    /// LSN of byte 0 of this WAL file. Raised by every checkpoint.
    pub base_lsn: u64,
}

impl WalHeader {
    /// Writes the header into the first [`HEADER_SIZE`] bytes of `buffer`.
    ///
    /// # Errors
    ///
    /// Returns a [`WalError::Page`] out-of-bounds error if `buffer` is shorter
    /// than [`HEADER_SIZE`]. The buffer is not modified in that case.
    pub fn encode(&self, buffer: &mut [u8]) -> Result<(), WalError> {
        slice(buffer, 0, HEADER_SIZE)?;
        write_bytes(buffer, 0, &[0; HEADER_SIZE])?;
        write_bytes(buffer, 0, &MAGIC)?;
        write_u16(buffer, OFF_VERSION, VERSION)?;
        write_u32(buffer, OFF_PAGE_SIZE, self.page_size.get())?;
        write_u64(buffer, OFF_BASE_LSN, self.base_lsn)?;
        let crc = crc32(slice(buffer, 0, OFF_HEADER_CRC)?);
        write_u32(buffer, OFF_HEADER_CRC, crc)?;
        Ok(())
    }

    /// Reads and validates the header from the start of `buffer`.
    ///
    /// # Errors
    ///
    /// Checks in this order and returns the first failure:
    ///
    /// 1. [`WalError::CorruptHeader`] if `buffer` is shorter than [`HEADER_SIZE`].
    /// 2. [`WalError::BadMagic`] if the magic number does not match.
    /// 3. [`WalError::UnsupportedVersion`] if the version is not [`VERSION`].
    /// 4. [`WalError::CorruptHeader`] if the CRC32 does not match.
    /// 5. [`WalError::Page`] if the page size is not valid (ADR 0001).
    pub fn decode(buffer: &[u8]) -> Result<Self, WalError> {
        let header = buffer.get(..HEADER_SIZE).ok_or(WalError::CorruptHeader)?;

        let magic = <[u8; 8]>::try_from(slice(header, 0, MAGIC.len())?)
            .map_err(|_| WalError::CorruptHeader)?;
        if magic != MAGIC {
            return Err(WalError::BadMagic { found: magic });
        }

        let version = read_u16(header, OFF_VERSION)?;
        if version != VERSION {
            return Err(WalError::UnsupportedVersion { version });
        }

        if read_u32(header, OFF_HEADER_CRC)? != crc32(slice(header, 0, OFF_HEADER_CRC)?) {
            return Err(WalError::CorruptHeader);
        }

        Ok(Self {
            page_size: PageSize::new(read_u32(header, OFF_PAGE_SIZE)?)?,
            base_lsn: read_u64(header, OFF_BASE_LSN)?,
        })
    }
}

/// Encodes one `PageImages` record into `out`, replacing its contents.
///
/// `lsn` must be `base_lsn + offset` of the place where the record will be
/// written, and every image must already carry `page_lsn == lsn` and a valid
/// checksum: the encoder copies the images as given. Pages should be in
/// ascending id order. On error `out` is left empty.
///
/// # Errors
///
/// - [`WalError::EmptyRecord`] if `pages` is empty.
/// - [`WalError::ImageLength`] if an image is not exactly `page_size` bytes.
/// - [`WalError::RecordTooLarge`] if `total_len` does not fit in a `u32`.
pub fn encode_record<'a>(
    out: &mut Vec<u8>,
    lsn: u64,
    page_size: PageSize,
    pages: impl ExactSizeIterator<Item = (PageId, &'a [u8])>,
) -> Result<(), WalError> {
    out.clear();
    let result = encode_into(out, lsn, page_size, pages);
    if result.is_err() {
        out.clear();
    }
    result
}

fn encode_into<'a>(
    out: &mut Vec<u8>,
    lsn: u64,
    page_size: PageSize,
    pages: impl ExactSizeIterator<Item = (PageId, &'a [u8])>,
) -> Result<(), WalError> {
    let count = pages.len();
    if count == 0 {
        return Err(WalError::EmptyRecord);
    }
    let total_len = count
        .checked_mul(page_size.as_usize().saturating_add(4))
        .and_then(|n| n.checked_add(RECORD_HEADER_SIZE))
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(WalError::RecordTooLarge)?;
    let count = u32::try_from(count).map_err(|_| WalError::RecordTooLarge)?;

    out.resize(RECORD_HEADER_SIZE, 0);
    for (id, image) in pages {
        if image.len() != page_size.as_usize() {
            return Err(WalError::ImageLength {
                expected: page_size.as_usize(),
                found: image.len(),
            });
        }
        out.extend_from_slice(&id.get().to_le_bytes());
        out.extend_from_slice(image);
    }

    write_u32(out, OFF_REC_LEN, total_len)?;
    write_u64(out, OFF_REC_LSN, lsn)?;
    write_u8(out, OFF_REC_KIND, KIND_PAGE_IMAGES)?;
    write_u32(out, OFF_REC_COUNT, count)?;
    let crc = crc32(slice(
        out,
        OFF_REC_LEN,
        out.len().saturating_sub(OFF_REC_LEN),
    )?);
    write_u32(out, 0, crc)?;
    Ok(())
}

/// A valid record, borrowed from the WAL bytes.
#[derive(Debug, Clone, Copy)]
pub struct RecordRef<'a> {
    lsn: u64,
    offset: usize,
    len: usize,
    page_size: usize,
    count: u32,
    images: &'a [u8],
}

impl<'a> RecordRef<'a> {
    /// LSN of the record (`base_lsn + offset`).
    #[must_use]
    pub const fn lsn(&self) -> u64 {
        self.lsn
    }

    /// Byte offset of the record in the WAL file.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }

    /// Total length of the record in bytes.
    #[must_use]
    pub const fn byte_len(&self) -> usize {
        self.len
    }

    /// Number of page images in the record.
    #[must_use]
    pub const fn page_count(&self) -> u32 {
        self.count
    }

    /// Iterates over `(page id, image)` pairs, borrowing from the WAL bytes.
    #[must_use]
    pub const fn pages(&self) -> PageImages<'a> {
        PageImages {
            rest: self.images,
            page_size: self.page_size,
            remaining: self.count,
        }
    }
}

/// Iterator over the page images of a [`RecordRef`].
#[derive(Debug, Clone)]
pub struct PageImages<'a> {
    rest: &'a [u8],
    page_size: usize,
    remaining: u32,
}

impl<'a> Iterator for PageImages<'a> {
    type Item = (PageId, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let (entry, tail) = self.rest.split_at_checked(self.page_size.checked_add(4)?)?;
        let id = read_u32(entry, 0).ok()?;
        let image = entry.get(4..)?;
        self.rest = tail;
        self.remaining = self.remaining.saturating_sub(1);
        Some((PageId::new(id), image))
    }
}

/// Reads the valid records of a WAL file, in order, without copying.
///
/// The first record that fails any validity check ends the log: it is a torn
/// tail, not an error (ADR 0006). Checks, in order: length fields, fits in the
/// file, `lsn == base_lsn + offset`, CRC32, known kind, and every image has a
/// valid page checksum with `page_lsn == lsn`.
#[derive(Debug, Clone)]
pub struct LogReader<'a> {
    bytes: &'a [u8],
    header: WalHeader,
    offset: usize,
}

impl<'a> LogReader<'a> {
    /// Starts reading the whole WAL file `bytes`, header included.
    ///
    /// # Errors
    ///
    /// Any error of [`WalHeader::decode`].
    pub fn new(bytes: &'a [u8]) -> Result<Self, WalError> {
        Ok(Self {
            bytes,
            header: WalHeader::decode(bytes)?,
            offset: HEADER_SIZE,
        })
    }

    /// The decoded WAL header.
    #[must_use]
    pub const fn header(&self) -> WalHeader {
        self.header
    }

    /// Offset just past the last valid record returned so far. After the
    /// iterator returns `None`, this is where the valid log ends.
    #[must_use]
    pub const fn valid_end(&self) -> usize {
        self.offset
    }

    fn parse(&self) -> Option<RecordRef<'a>> {
        let offset = self.offset;
        let rest = self.bytes.get(offset..)?;
        let page_size = self.header.page_size.as_usize();

        let total_len = usize::try_from(read_u32(rest, OFF_REC_LEN).ok()?).ok()?;
        let count = read_u32(rest, OFF_REC_COUNT).ok()?;
        let expected = usize::try_from(count)
            .ok()?
            .checked_mul(page_size.checked_add(4)?)?
            .checked_add(RECORD_HEADER_SIZE)?;
        if count == 0 || total_len != expected {
            return None;
        }
        let record = rest.get(..total_len)?;

        let lsn = read_u64(record, OFF_REC_LSN).ok()?;
        if lsn
            != self
                .header
                .base_lsn
                .checked_add(u64::try_from(offset).ok()?)?
        {
            return None;
        }
        if read_u32(record, 0).ok()? != crc32(record.get(OFF_REC_LEN..)?) {
            return None;
        }
        if read_u8(record, OFF_REC_KIND).ok()? != KIND_PAGE_IMAGES {
            return None;
        }

        let found = RecordRef {
            lsn,
            offset,
            len: total_len,
            page_size,
            count,
            images: record.get(RECORD_HEADER_SIZE..)?,
        };
        for (_, image) in found.pages() {
            verify_checksum(image).ok()?;
            if page_lsn(image).ok()? != lsn {
                return None;
            }
        }
        Some(found)
    }
}

impl<'a> Iterator for LogReader<'a> {
    type Item = RecordRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let record = self.parse()?;
        self.offset = self.offset.checked_add(record.len)?;
        Some(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::pager::page::{set_page_lsn, stamp_checksum};
    use anyhow::{Context, Result, ensure};
    use proptest::prelude::*;

    const PS: usize = 512;

    fn page_size() -> Result<PageSize> {
        Ok(PageSize::new(512)?)
    }

    fn header_bytes(base_lsn: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0; HEADER_SIZE];
        WalHeader {
            page_size: page_size()?,
            base_lsn,
        }
        .encode(&mut buf)?;
        Ok(buf)
    }

    fn image(fill: u8, lsn: u64) -> Result<Vec<u8>> {
        let mut img = vec![fill; PS];
        set_page_lsn(&mut img, lsn)?;
        stamp_checksum(&mut img)?;
        Ok(img)
    }

    /// Appends a record at the end of `file` with the given `lsn` and images.
    fn append(file: &mut Vec<u8>, lsn: u64, images: &[Vec<u8>]) -> Result<()> {
        let mut rec = Vec::new();
        let pages = images.iter().enumerate().map(|(i, img)| {
            (
                PageId::new(u32::try_from(i).unwrap_or(0).saturating_add(1)),
                img.as_slice(),
            )
        });
        encode_record(&mut rec, lsn, page_size()?, pages)?;
        file.extend_from_slice(&rec);
        Ok(())
    }

    /// Builds a valid WAL. `specs[k]` holds the fill bytes of record k's images.
    /// Returns the file and the end offset of every record.
    fn build(base_lsn: u64, specs: &[Vec<u8>]) -> Result<(Vec<u8>, Vec<usize>)> {
        let mut file = header_bytes(base_lsn)?;
        let mut ends = Vec::new();
        for spec in specs {
            let lsn = base_lsn.saturating_add(u64::try_from(file.len())?);
            let images = spec
                .iter()
                .map(|f| image(*f, lsn))
                .collect::<Result<Vec<_>>>()?;
            append(&mut file, lsn, &images)?;
            ends.push(file.len());
        }
        Ok((file, ends))
    }

    type Owned = Vec<(u64, Vec<(u32, Vec<u8>)>)>;

    fn read_all(file: &[u8]) -> Result<(Owned, usize)> {
        let mut reader = LogReader::new(file)?;
        let mut out = Vec::new();
        for rec in reader.by_ref() {
            let pages = rec
                .pages()
                .map(|(id, img)| (id.get(), img.to_vec()))
                .collect();
            out.push((rec.lsn(), pages));
        }
        Ok((out, reader.valid_end()))
    }

    fn count_valid(file: &[u8]) -> Result<usize> {
        Ok(read_all(file)?.0.len())
    }

    fn sample() -> Result<(Vec<u8>, Vec<usize>)> {
        build(100, &[vec![1], vec![2, 3], vec![4]])
    }

    fn to_test(r: Result<()>) -> Result<(), TestCaseError> {
        r.map_err(|e| TestCaseError::fail(format!("{e:#}")))
    }

    proptest! {
        #[test]
        fn header_roundtrip(exp in 9u32..=16, base_lsn in any::<u64>()) {
            to_test((|| {
                let header = WalHeader { page_size: PageSize::new(1 << exp)?, base_lsn };
                let mut buf = vec![0xAA; HEADER_SIZE + 5];
                header.encode(&mut buf)?;
                ensure!(WalHeader::decode(&buf)? == header);
                Ok(())
            })())?;
        }

        #[test]
        fn records_roundtrip(
            base_lsn in 0..u64::MAX / 2,
            specs in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..=3), 0..=4),
        ) {
            to_test((|| {
                let (file, ends) = build(base_lsn, &specs)?;
                let (records, valid_end) = read_all(&file)?;
                ensure!(records.len() == specs.len());
                ensure!(valid_end == file.len());
                let mut start = HEADER_SIZE;
                for ((rec, spec), end) in records.iter().zip(&specs).zip(&ends) {
                    ensure!(rec.0 == base_lsn.saturating_add(u64::try_from(start)?));
                    ensure!(rec.1.len() == spec.len());
                    for (i, ((id, img), fill)) in rec.1.iter().zip(spec).enumerate() {
                        ensure!(usize::try_from(*id)? == i.saturating_add(1));
                        ensure!(*img == image(*fill, rec.0)?);
                    }
                    start = *end;
                }
                Ok(())
            })())?;
        }
    }

    #[test]
    fn truncation_at_every_byte_keeps_exactly_the_whole_records() -> Result<()> {
        let (file, ends) = sample()?;
        for cut in HEADER_SIZE..=file.len() {
            let expected = ends.iter().filter(|e| **e <= cut).count();
            let (records, valid_end) = read_all(file.get(..cut).context("cut")?)?;
            ensure!(records.len() == expected, "cut {cut}");
            let end = expected.checked_sub(1).and_then(|k| ends.get(k)).copied();
            ensure!(valid_end == end.unwrap_or(HEADER_SIZE), "cut {cut}");
        }
        Ok(())
    }

    #[test]
    fn bit_flip_in_record_k_leaves_only_earlier_records() -> Result<()> {
        let (file, ends) = sample()?;
        let mut start = HEADER_SIZE;
        for (k, end) in ends.iter().enumerate() {
            for pos in start..*end {
                for mask in [0x01, 0x80] {
                    let mut bad = file.clone();
                    *bad.get_mut(pos).context("pos")? ^= mask;
                    ensure!(
                        count_valid(&bad)? == k,
                        "record {k}, byte {pos}, mask {mask}"
                    );
                }
            }
            start = *end;
        }
        Ok(())
    }

    #[test]
    fn stale_lsn_ends_the_log() -> Result<()> {
        // A record written when the base was lower: lsn = old_base + offset.
        let mut file = header_bytes(100)?;
        let stale = 90u64.saturating_add(u64::try_from(file.len())?);
        append(&mut file, stale, &[image(7, stale)?])?;
        let (records, valid_end) = read_all(&file)?;
        ensure!(records.is_empty() && valid_end == HEADER_SIZE);
        Ok(())
    }

    #[test]
    fn bad_image_ends_the_log() -> Result<()> {
        let lsn = u64::try_from(HEADER_SIZE)?;
        let mut bad_checksum = image(5, lsn)?;
        *bad_checksum.get_mut(100).context("byte")? ^= 1;
        let wrong_lsn = image(5, lsn.saturating_add(1))?;
        for bad in [bad_checksum, wrong_lsn] {
            let mut file = header_bytes(0)?;
            // The record CRC is valid: only the image check can reject it.
            append(&mut file, lsn, &[bad])?;
            ensure!(count_valid(&file)? == 0);
        }
        Ok(())
    }

    #[test]
    fn bad_headers_are_errors() -> Result<()> {
        let good = header_bytes(7)?;
        for pos in 0..HEADER_SIZE {
            let mut bad = good.clone();
            *bad.get_mut(pos).context("pos")? ^= 1;
            ensure!(WalHeader::decode(&bad).is_err(), "byte {pos}");
        }
        let mut bad = good.clone();
        *bad.get_mut(0).context("pos")? ^= 1;
        ensure!(matches!(
            WalHeader::decode(&bad),
            Err(WalError::BadMagic { .. })
        ));
        let mut bad = good.clone();
        *bad.get_mut(OFF_VERSION).context("pos")? = 2;
        ensure!(matches!(
            WalHeader::decode(&bad),
            Err(WalError::UnsupportedVersion { version: 2 })
        ));
        let mut bad = good.clone();
        *bad.get_mut(16).context("pos")? ^= 1;
        ensure!(matches!(
            WalHeader::decode(&bad),
            Err(WalError::CorruptHeader)
        ));
        ensure!(matches!(
            WalHeader::decode(good.get(..31).context("len")?),
            Err(WalError::CorruptHeader)
        ));
        ensure!(LogReader::new(&[]).is_err());
        Ok(())
    }

    #[test]
    fn encode_rejects_bad_input() -> Result<()> {
        let mut out = vec![9];
        let short = vec![0u8; PS - 1];
        let err = encode_record(
            &mut out,
            32,
            page_size()?,
            std::iter::once((PageId::new(1), short.as_slice())),
        );
        ensure!(matches!(err, Err(WalError::ImageLength { .. })) && out.is_empty());
        let err = encode_record(&mut out, 32, page_size()?, std::iter::empty());
        ensure!(matches!(err, Err(WalError::EmptyRecord)));
        Ok(())
    }
}
