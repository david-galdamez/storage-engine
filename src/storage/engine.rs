//! The storage engine interface.
//!
//! It is engine-agnostic: the B+Tree engine
//! ([`BTreeEngine`](crate::storage::btree::BTreeEngine)) implements it, and
//! another engine, such as an LSM tree, could implement it later.
//! [`StorageEngine::scan`] returns a lending cursor ([`EngineCursor`]) over a
//! key range, in either [`Direction`].
//!
//! The unit of work is a tree (keyspace): a model's primary data or one
//! secondary index. Keys are unique byte strings compared in `memcmp` order.
//!
//! A tree handle is only valid on the engine that created or opened it. Using
//! a handle on another engine may return an error or wrong data, but never
//! panics.

use std::ops::Bound;

/// The order in which a scan returns keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Ascending key order, from the lower bound to the upper bound.
    Forward,
    /// Descending key order, from the upper bound to the lower bound.
    Reverse,
}

/// A lending cursor over a key range, created by [`StorageEngine::scan`].
///
/// Each call lends the next key, so the cursor cannot be an [`Iterator`]. The
/// values are copied into a buffer owned by the caller, and only when the
/// caller asks for them (ADR 0005, option 7).
pub trait EngineCursor {
    /// Error type of the cursor. It is the error type of the engine.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Returns the next key of the scan, or `None` at the end of the range.
    ///
    /// The key is lent until the next call. If `value` is `Some`, the buffer
    /// is cleared at the top of every call. After `Ok(Some(key))` it holds
    /// exactly the value of that key, and after `Ok(None)` or an error it is
    /// empty. If `value` is `None`, the buffer is not touched and the engine
    /// reads no value (for the B+Tree, no overflow page).
    ///
    /// The cursor is fused: after `Ok(None)` or any error, every later call
    /// returns `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage cannot be read, if its contents are
    /// corrupt, or if the value of the key cannot be read.
    fn next_entry(&mut self, value: Option<&mut Vec<u8>>) -> Result<Option<&[u8]>, Self::Error>;
}

/// A store of ordered key-value trees.
pub trait StorageEngine {
    /// Handle of one tree (keyspace): a model's primary data or one index.
    type Tree: Copy;

    /// Error type of the engine.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Cursor of a scan. It borrows the engine, so the engine cannot be
    /// written while a cursor is alive.
    type Cursor<'a>: EngineCursor<Error = Self::Error>
    where
        Self: 'a;

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

    /// Opens a cursor over the keys of `tree` between `lower` and `upper`, in
    /// `dir` order. The bounds are borrowed and must outlive the cursor. An
    /// empty or inverted range is not an error: the cursor returns nothing.
    ///
    /// The cursor borrows the engine, so no `put` or `delete` can run while it
    /// is alive:
    ///
    /// ```compile_fail,E0502
    /// # use std::ops::Bound;
    /// # use storage_engine_rs::storage::engine::{Direction, EngineCursor, StorageEngine};
    /// fn demo<E: StorageEngine>(engine: &mut E, tree: E::Tree) -> Result<(), E::Error> {
    ///     let mut cursor =
    ///         engine.scan(tree, Bound::Unbounded, Bound::Unbounded, Direction::Forward)?;
    ///     engine.put(tree, b"key", b"value")?;
    ///     cursor.next_entry(None)?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// A key is lent until the next call, so it cannot be kept across calls:
    ///
    /// ```compile_fail,E0499
    /// # use std::ops::Bound;
    /// # use storage_engine_rs::storage::engine::{Direction, EngineCursor, StorageEngine};
    /// fn demo<E: StorageEngine>(engine: &E, tree: E::Tree) -> Result<(), E::Error> {
    ///     let mut cursor =
    ///         engine.scan(tree, Bound::Unbounded, Bound::Unbounded, Direction::Forward)?;
    ///     let first = cursor.next_entry(None)?;
    ///     let second = cursor.next_entry(None)?;
    ///     assert_eq!(first, second);
    ///     Ok(())
    /// }
    /// ```
    ///
    /// A loop over all entries, with the values in one reused buffer:
    ///
    /// ```
    /// # use std::ops::Bound;
    /// # use storage_engine_rs::storage::engine::{Direction, EngineCursor, StorageEngine};
    /// fn total_len<E: StorageEngine>(engine: &E, tree: E::Tree) -> Result<usize, E::Error> {
    ///     let mut cursor =
    ///         engine.scan(tree, Bound::Unbounded, Bound::Unbounded, Direction::Forward)?;
    ///     let mut value = Vec::new();
    ///     let mut total = 0usize;
    ///     while let Some(key) = cursor.next_entry(Some(&mut value))? {
    ///         total = total.saturating_add(key.len()).saturating_add(value.len());
    ///     }
    ///     Ok(total)
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the storage cannot be read, if its contents are
    /// corrupt, or if `tree` is not a tree of this engine.
    fn scan<'a>(
        &'a self,
        tree: Self::Tree,
        lower: Bound<&'a [u8]>,
        upper: Bound<&'a [u8]>,
        dir: Direction,
    ) -> Result<Self::Cursor<'a>, Self::Error>;
}
