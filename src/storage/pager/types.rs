//! Core pager types such as page identifiers.

use crate::storage::pager::error::PageError;

/// Number of a page in the database file. Page N starts at byte offset N * page size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId(u32);

impl PageId {
    /// Page 0, which holds the file header (ADR 0003).
    pub const HEADER: Self = Self(0);

    /// Creates a page id from its raw page number.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// Returns the raw page number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Returns the byte offset of this page in the file (page id * page size).
    #[must_use]
    pub fn offset(self, page_size: PageSize) -> u64 {
        let page_id = u64::from(self.0);
        let size = u64::from(page_size.size);
        // Never saturates: page_id < 2^32 and size <= 2^16, so the product is
        // below 2^48, far from u64::MAX.
        page_id.saturating_mul(size)
    }

    /// Decodes an on-disk page reference. 0 means "no page" and becomes `None`,
    /// because page 0 (the file header) is never referenced by a link.
    #[must_use]
    pub const fn from_disk(id: u32) -> Option<Self> {
        if id == 0 { None } else { Some(Self::new(id)) }
    }

    /// Encodes an optional page reference for disk. `None` becomes 0.
    #[must_use]
    pub const fn to_disk(page_id: Option<Self>) -> u32 {
        if let Some(id) = page_id {
            return id.0;
        }
        0
    }
}

/// Formats the raw page number.
impl std::fmt::Display for PageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Validated page size: a power of two in `[512, 65536]` (ADR 0001).
/// A value of this type is always valid.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PageSize {
    size: u32,
    len: usize,
}

impl PageSize {
    /// Smallest allowed page size in bytes.
    pub const MIN: u32 = 512;
    /// Largest allowed page size in bytes.
    pub const MAX: u32 = 65536;
    /// Default page size in bytes (4 KiB).
    pub const DEFAULT: u32 = 4096;

    /// Validates `size` and creates a page size.
    ///
    /// # Errors
    ///
    /// Will return Err if size is not a power of two or if size is not in the 512-65536 range
    pub fn new(size: u32) -> Result<Self, PageError> {
        if !size.is_power_of_two() {
            return Err(PageError::InvalidPageSize { size });
        }

        if !(Self::MIN..=Self::MAX).contains(&size) {
            return Err(PageError::InvalidPageSize { size });
        }

        let len: usize = size
            .try_into()
            .map_err(|_| PageError::InvalidPageSize { size })?;

        Ok(Self { size, len })
    }

    /// Returns the page size in bytes, as stored in the file header.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.size
    }

    /// Returns the page size in bytes as `usize`, for buffer lengths and slicing.
    #[must_use]
    pub const fn as_usize(self) -> usize {
        self.len
    }
}

/// Returns a 4 KiB page size, see ADR 0001.
impl Default for PageSize {
    fn default() -> Self {
        Self {
            size: Self::DEFAULT,
            len: usize::from(4096_u16),
        }
    }
}
