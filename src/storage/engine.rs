//! The storage engine interface.
//!
//! It is engine-agnostic: the B+Tree engine
//! ([`BTreeEngine`](crate::storage::btree::BTreeEngine)) implements it, and
//! another engine, such as an LSM tree, could implement it later. `scan` lands
//! in Phase 2 step 10c: the B+Tree already has a lending cursor
//! (`BTree::range`), which `scan` will wrap.
//!
//! The unit of work is a tree (keyspace): a model's primary data or one
//! secondary index. Keys are unique byte strings compared in `memcmp` order.
//!
//! A tree handle is only valid on the engine that created or opened it. Using
//! a handle on another engine may return an error or wrong data, but never
//! panics.

/// A store of ordered key-value trees.
pub trait StorageEngine {
    /// Handle of one tree (keyspace): a model's primary data or one index.
    type Tree: Copy;

    /// Error type of the engine.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Creates a new empty tree and returns its handle.
    ///
    /// # Errors
    ///
    /// Returns an error if the engine cannot allocate or write the storage
    /// for the new tree.
    fn create_tree(&mut self) -> Result<Self::Tree, Self::Error>;

    /// Copies the value of `key` into `out` and returns whether the key
    /// exists. `out` is cleared first. It is owned by the caller, so a loop of
    /// lookups allocates once (ADR 0005, option 7).
    ///
    /// Keys are unique byte strings in `memcmp` order. The engine sets the
    /// size limits.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage cannot be read, if its contents are
    /// corrupt, or if `tree` is not a tree of this engine.
    fn get(&self, tree: Self::Tree, key: &[u8], out: &mut Vec<u8>) -> Result<bool, Self::Error>;

    /// Inserts `key` with `value`, or replaces the value if the key exists.
    /// Returns whether a previous value was replaced.
    ///
    /// Keys are unique byte strings in `memcmp` order. The engine sets the
    /// size limits for keys and values.
    ///
    /// # Errors
    ///
    /// Returns an error if the key or value exceeds the limits of the engine,
    /// if the storage cannot be read or written, or if its contents are
    /// corrupt.
    fn put(&mut self, tree: Self::Tree, key: &[u8], value: &[u8]) -> Result<bool, Self::Error>;

    /// Removes `key`. Returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage cannot be read or written, or if its
    /// contents are corrupt.
    fn delete(&mut self, tree: Self::Tree, key: &[u8]) -> Result<bool, Self::Error>;
}
