//! Write-ahead log: on-disk format of the WAL file (ADR 0006).
//!
//! `format` is the byte-level codec; `log` is the WAL file itself.

pub mod error;
pub mod format;
pub mod log;
