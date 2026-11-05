//! Pager: fixed-size page I/O over a single database file.
//!
//! The file format is described in ADR 0003 (`docs/decisions/0003-pager-file-format.md`).

pub mod codec;
pub mod crc32;
pub mod disk;
pub mod error;
pub mod file_header;
pub mod freelist;
pub mod page;
pub mod slotted;
pub mod types;
