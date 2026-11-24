//! Storage layer: the engine trait, the B+Tree engine, the in-memory store and
//! the on-disk pager.

pub mod btree;
pub mod engine;
pub mod memory;
pub mod pager;

pub use memory::Storage;
