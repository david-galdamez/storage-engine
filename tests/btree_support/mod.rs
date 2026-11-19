//! Shared helpers for the B+Tree integration tests.
//!
//! Splits do not exist yet, so the tests build trees by hand with the public
//! node API: `write_leaf`, `write_internal` and a small bottom-up bulk loader.
//! The root of a hand-built tree is not the page `BTree::create` made, so the
//! tests use `BTree::open(pager, root)` to get the handle.
//!
//! Each test binary that includes this module with `mod btree_support;` must
//! use every item, or `dead_code` fires.

use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::btree::error::NodeError;
use storage_engine_rs::storage::btree::internal::{InternalNode, InternalNodeMut};
use storage_engine_rs::storage::btree::leaf::LeafNodeMut;
use storage_engine_rs::storage::btree::node::ValueRef;
use storage_engine_rs::storage::pager::disk::{Pager, PagerOptions};
use storage_engine_rs::storage::pager::error::PageError;
use storage_engine_rs::storage::pager::types::{PageId, PageSize};
use tempfile::TempDir;

pub const PAGE_SIZE: usize = 512;

/// Maximum number of children the bulk loader gives an internal node. Small,
/// so that a few hundred entries already make three levels.
const FANOUT: usize = 4;

pub fn page_size() -> anyhow::Result<PageSize> {
    Ok(PageSize::new(u32::try_from(PAGE_SIZE)?)?)
}

pub fn temp_db() -> anyhow::Result<(TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("test.db");
    Ok((dir, path))
}

pub fn new_pager(path: &Path) -> anyhow::Result<Pager> {
    Ok(Pager::create(
        path,
        PagerOptions {
            page_size: page_size()?,
        },
    )?)
}

pub fn blank() -> Vec<u8> {
    vec![0u8; PAGE_SIZE]
}

/// Formats `id` as a leaf with `entries` (sorted, unique) and writes it.
pub fn write_leaf_at(
    pager: &mut Pager,
    id: PageId,
    entries: &[(&[u8], &[u8])],
    next: Option<PageId>,
) -> anyhow::Result<()> {
    let mut buf = blank();
    LeafNodeMut::init(&mut buf)?;
    let mut leaf = LeafNodeMut::new(&mut buf)?;
    for (index, (key, value)) in entries.iter().enumerate() {
        leaf.insert(index, key, ValueRef::Inline(value))?;
    }
    leaf.set_next_leaf(next)?;
    pager.write_page(id, &mut buf)?;
    Ok(())
}

pub fn write_leaf(
    pager: &mut Pager,
    entries: &[(&[u8], &[u8])],
    next: Option<PageId>,
) -> anyhow::Result<PageId> {
    let id = pager.allocate_page()?;
    write_leaf_at(pager, id, entries, next)?;
    Ok(id)
}

/// Formats `id` as an internal node and writes it. `cells` are
/// `(child, separator)` pairs in order.
pub fn write_internal_at(
    pager: &mut Pager,
    id: PageId,
    cells: &[(PageId, &[u8])],
    rightmost: PageId,
) -> anyhow::Result<()> {
    let mut buf = blank();
    InternalNodeMut::init(&mut buf, rightmost)?;
    let mut node = InternalNodeMut::new(&mut buf)?;
    for (index, (child, key)) in cells.iter().enumerate() {
        node.insert(index, key, *child)?;
    }
    pager.write_page(id, &mut buf)?;
    Ok(())
}

pub fn write_internal(
    pager: &mut Pager,
    cells: &[(PageId, &[u8])],
    rightmost: PageId,
) -> anyhow::Result<PageId> {
    let id = pager.allocate_page()?;
    write_internal_at(pager, id, cells, rightmost)?;
    Ok(id)
}

/// Packs sorted unique `entries` into linked leaves, then builds internal
/// levels bottom-up until one root remains. The separator for a right sibling
/// is the first key of its subtree (ADR 0005, invariants 4 and 5).
pub fn bulk_load(pager: &mut Pager, entries: &[(Vec<u8>, Vec<u8>)]) -> anyhow::Result<PageId> {
    // Group the entries by trial insertion into a scratch leaf.
    let mut groups: Vec<Vec<&(Vec<u8>, Vec<u8>)>> = Vec::new();
    let mut scratch = blank();
    LeafNodeMut::init(&mut scratch)?;
    let mut current: Vec<&(Vec<u8>, Vec<u8>)> = Vec::new();
    for entry in entries {
        let inserted = {
            let mut leaf = LeafNodeMut::new(&mut scratch)?;
            let index = current.len();
            leaf.insert(index, &entry.0, ValueRef::Inline(&entry.1))
        };
        match inserted {
            Ok(()) => {}
            Err(NodeError::Page(PageError::PageFull { .. })) => {
                groups.push(std::mem::take(&mut current));
                LeafNodeMut::init(&mut scratch)?;
                LeafNodeMut::new(&mut scratch)?.insert(0, &entry.0, ValueRef::Inline(&entry.1))?;
            }
            Err(other) => return Err(other.into()),
        }
        current.push(entry);
    }
    groups.push(current);

    let ids = groups
        .iter()
        .map(|_| pager.allocate_page())
        .collect::<Result<Vec<_>, _>>()?;

    // Each level is a list of (first key of the subtree, page).
    let mut level: Vec<(Vec<u8>, PageId)> = Vec::new();
    for (position, group) in groups.iter().enumerate() {
        let id = *ids.get(position).context("leaf id")?;
        let next = ids
            .get(position.checked_add(1).context("overflow")?)
            .copied();
        let cells: Vec<(&[u8], &[u8])> = group
            .iter()
            .map(|entry| (entry.0.as_slice(), entry.1.as_slice()))
            .collect();
        write_leaf_at(pager, id, &cells, next)?;
        let first = group.first().context("empty leaf group")?;
        level.push((first.0.clone(), id));
    }

    while level.len() > 1 {
        let mut chunks: Vec<&[(Vec<u8>, PageId)]> = level.chunks(FANOUT).collect();
        // A node needs at least two children: fold a lone last child into the
        // previous chunk.
        let lone_last = chunks.last().is_some_and(|last| last.len() == 1);
        let mut merged: Vec<(Vec<u8>, PageId)> = Vec::new();
        if lone_last && chunks.len() > 1 {
            chunks.pop();
            let last_start = chunks.len().saturating_sub(1);
            let tail_from = last_start.checked_mul(FANOUT).context("overflow")?;
            merged = level.get(tail_from..).context("tail")?.to_vec();
            chunks.pop();
        }

        let mut next_level = Vec::new();
        let mut build = |children: &[(Vec<u8>, PageId)]| -> anyhow::Result<()> {
            let (last, init) = children.split_last().context("empty chunk")?;
            let cells: Vec<(PageId, &[u8])> = init
                .iter()
                .zip(children.iter().skip(1))
                .map(|((_, child), (next_first, _))| (*child, next_first.as_slice()))
                .collect();
            let id = write_internal(pager, &cells, last.1)?;
            let first = children.first().context("empty chunk")?;
            next_level.push((first.0.clone(), id));
            Ok(())
        };
        for chunk in &chunks {
            build(chunk)?;
        }
        if !merged.is_empty() {
            build(&merged)?;
        }
        level = next_level;
    }

    Ok(level.first().context("no root")?.1)
}

/// Number of levels from the root down to a leaf, following child 0.
pub fn height(pager: &Pager, root: PageId) -> anyhow::Result<usize> {
    let mut id = root;
    let mut levels = 1usize;
    loop {
        let mut buf = blank();
        pager.read_page(id, &mut buf)?;
        match InternalNode::new(&buf) {
            Ok(node) => {
                id = node.child_at(0)?;
                levels = levels.checked_add(1).context("overflow")?;
            }
            Err(_) => return Ok(levels),
        }
    }
}

pub fn get(tree: BTree, pager: &Pager, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    Ok(tree.get(pager, key, &mut out)?.then_some(out))
}

pub fn ensure_value(got: Option<&[u8]>, expected: &[u8], what: &str) -> anyhow::Result<()> {
    ensure!(
        got == Some(expected),
        "{what}: expected {expected:?}, got {got:?}"
    );
    Ok(())
}

pub fn ensure_missing(got: Option<&[u8]>, what: &str) -> anyhow::Result<()> {
    ensure!(got.is_none(), "{what}: expected a miss, got {got:?}");
    Ok(())
}

pub fn numbered_entries(count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..count)
        .map(|i| {
            (
                format!("key-{:05}", i.saturating_mul(2)).into_bytes(),
                format!("value-{i}").into_bytes(),
            )
        })
        .collect()
}
