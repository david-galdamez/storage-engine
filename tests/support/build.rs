//! Hand-built trees for the B+Tree integration tests.
//!
//! The tests build trees with the public node API: `write_leaf`,
//! `write_internal` and a small bottom-up bulk loader. The root of a
//! hand-built tree is not the page `BTree::create` made, so the tests use
//! `BTree::open(pager, root)` to get the handle.
//!
//! Included with `#[path = "support/build.rs"] mod build;`. Every binary that
//! includes it must use all of it, or `dead_code` fires.

use anyhow::Context;
use storage_engine_rs::storage::btree::error::NodeError;
use storage_engine_rs::storage::btree::internal::InternalNodeMut;
use storage_engine_rs::storage::btree::leaf::LeafNodeMut;
use storage_engine_rs::storage::btree::node::ValueRef;
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::error::PageError;
use storage_engine_rs::storage::pager::types::PageId;

use crate::shape::blank;

/// Maximum number of children the bulk loader gives an internal node. Small,
/// so that a few hundred entries already make three levels.
const FANOUT: usize = 4;

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
