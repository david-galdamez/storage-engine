//! Write-ahead log: on-disk format of the WAL file (ADR 0006).
//!
//! This module holds the byte-level codec only. File I/O comes in later steps.

pub mod error;
pub mod format;
