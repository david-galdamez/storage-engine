//! [`BTreeEngine`]: the [`StorageEngine`] implementation over a [`Pager`].
//!
//! The engine owns the pager (it is moved in, not cloned or shared), and a
//! tree handle is a [`BTree`], a root page id. Every operation writes straight
//! to the file through the pager: nothing is buffered in the engine.
//!
//! - [`BTreeEngine::sync`] is only an `fsync`; it does not flush anything
//!   else. Dropping the engine does not sync.
//! - There is no write-ahead log yet, so a crash in the middle of a split can
//!   still corrupt the file until the WAL arrives (Phase 3).
//! - [`StorageEngine::scan`] returns a [`BTreeScan`], which wraps the B+Tree
//!   [`Cursor`] and reads a value (inline or from an overflow chain) only when
//!   the caller passes a buffer for it.
//! - The file header `catalog_root` is not touched. Trees are found by their
//!   root page id with [`BTreeEngine::open_tree`].

use std::ops::Bound;

use crate::storage::{
    btree::{BTree, Cursor, error::BTreeError, overflow},
    engine::{Direction, EngineCursor, StorageEngine},
    pager::{disk::Pager, types::PageId},
};

/// A [`StorageEngine`] backed by B+Trees in one pager file.
#[derive(Debug)]
pub struct BTreeEngine {
    pager: Pager,
}

impl BTreeEngine {
    /// Wraps `pager`. The pager is moved in, so the engine is its only owner.
    #[must_use]
    pub const fn new(pager: Pager) -> Self {
        Self { pager }
    }

    /// Opens the existing tree whose root is `root`. See [`BTree::open`].
    ///
    /// # Errors
    ///
    /// Returns [`BTreeError::InvalidRoot`] if `root` cannot be a tree root, or
    /// a pager or node error if the root page cannot be read.
    pub fn open_tree(&self, root: PageId) -> Result<BTree, BTreeError> {
        BTree::open(&self.pager, root)
    }

    /// Flushes the file to stable storage (`fsync`). It does not write
    /// anything itself: every operation already wrote to the file.
    ///
    /// # Errors
    ///
    /// Returns [`BTreeError::Pager`] if the sync fails.
    pub fn sync(&self) -> Result<(), BTreeError> {
        Ok(self.pager.sync()?)
    }

    /// Borrows the pager under the engine.
    #[must_use]
    pub const fn pager(&self) -> &Pager {
        &self.pager
    }

    /// Consumes the engine and returns its pager. Does not sync.
    #[must_use]
    pub fn into_pager(self) -> Pager {
        self.pager
    }
}

impl StorageEngine for BTreeEngine {
    type Tree = BTree;
    type Error = BTreeError;
    type Cursor<'a> = BTreeScan<'a>;

    fn create_tree(&mut self) -> Result<Self::Tree, Self::Error> {
        BTree::create(&mut self.pager)
    }

    fn get(&self, tree: Self::Tree, key: &[u8], out: &mut Vec<u8>) -> Result<bool, Self::Error> {
        out.clear();
        tree.get(&self.pager, key, out)
    }

    fn put(&mut self, tree: Self::Tree, key: &[u8], value: &[u8]) -> Result<bool, Self::Error> {
        tree.put(&mut self.pager, key, value)
    }

    fn delete(&mut self, tree: Self::Tree, key: &[u8]) -> Result<bool, Self::Error> {
        tree.delete(&mut self.pager, key)
    }

    fn scan<'a>(
        &'a self,
        tree: Self::Tree,
        lower: Bound<&'a [u8]>,
        upper: Bound<&'a [u8]>,
        dir: Direction,
    ) -> Result<Self::Cursor<'a>, Self::Error> {
        tree.range(&self.pager, lower, upper, dir)
            .map(|cursor| BTreeScan {
                cursor,
                pager: &self.pager,
                done: false,
            })
    }
}

/// The [`EngineCursor`] of [`BTreeEngine`]: a B+Tree [`Cursor`] plus the
/// pager, to read overflow values on request.
///
/// It is fused. After the end of the range or any error, including an error
/// while reading a value, every later call returns `Ok(None)`.
#[derive(Debug)]
pub struct BTreeScan<'a> {
    cursor: Cursor<'a>,
    pager: &'a Pager,
    done: bool,
}

impl EngineCursor for BTreeScan<'_> {
    type Error = BTreeError;

    fn next_entry(
        &mut self,
        mut value: Option<&mut Vec<u8>>,
    ) -> Result<Option<&[u8]>, Self::Error> {
        if self.done {
            return Ok(None);
        }
        if let Some(buffer) = value.as_deref_mut() {
            buffer.clear();
        }

        let cell = match self.cursor.next_entry() {
            Ok(Some(cell)) => cell,
            Ok(None) => {
                self.done = true;
                return Ok(None);
            }
            Err(error) => {
                self.done = true;
                return Err(error);
            }
        };

        if let Some(buffer) = value
            && let Err(error) = overflow::read_value(self.pager, cell.value, buffer)
        {
            self.done = true;
            buffer.clear();
            return Err(error);
        }
        Ok(Some(cell.key))
    }
}
