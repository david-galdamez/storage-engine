//! State of one atomic operation (ADR 0006, D4).

use std::collections::BTreeMap;

use crate::storage::pager::{file_header::FileHeader, types::PageId};

/// The pages an operation has changed so far, held in memory until commit
/// (no-steal, ADR 0006; _Database Internals_, Ch. 5, "Steal and Force
/// Policies").
///
/// The map owns one copy of each changed page: see the "Owned data" note in
/// ADR 0006. It goes away with the buffer pool in Phase 4.
#[derive(Debug)]
pub(super) struct OpState {
    /// Changed pages (stamped images, page 0 included), in ascending id order.
    pub(super) dirty: BTreeMap<PageId, Vec<u8>>,
    /// The file header when the operation began, restored on abort. Its
    /// `page_count` is also the number of pages the database file really has
    /// until commit applies the growth.
    pub(super) header_before: FileHeader,
    /// LSN stamped into every page of this operation: where its record goes.
    pub(super) lsn: u64,
}
