//! Storage layer: the in-memory store and the on-disk pager.

pub mod btree;
pub mod memory;
pub mod pager;

pub use memory::Storage;
