//! Tests for the B+Tree handle (`storage::btree::tree`, Phase 2 step 3).
//!
//! A `BTree` stores only the root page id; every operation takes the pager.
//! These tests check:
//!
//! - `create` allocates one page and writes an empty leaf: type byte 4, no
//!   cells, `free_end == page_size`, `next_leaf == 0`, zero reserved bytes and
//!   a valid checksum, also after closing and reopening the file.
//! - Two trees get distinct roots, and `create` reuses a freed page.
//! - `open` accepts a created root (also after a reopen) and an internal node.
//! - `open` rejects page 0, pages at or past `page_count`, a free page, a
//!   slotted page, an overflow page, each with the exact error.
//! - `open` on a leaf with a corrupt slotted header gives
//!   `BTreeError::Node`.
//! - `BTree` is `Copy` and `root()` returns the page id.
//!
//! Expected bytes are written as literals (ADR 0003 and 0005), not taken from
//! the code under test. Page size is 512 bytes.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::btree::error::BTreeError;
use storage_engine_rs::storage::btree::internal::InternalNodeMut;
use storage_engine_rs::storage::pager::disk::{Pager, PagerOptions};
use storage_engine_rs::storage::pager::slotted::SlottedPageMut;
use storage_engine_rs::storage::pager::types::{PageId, PageSize};
use tempfile::TempDir;

const PAGE_SIZE: usize = 512;

// Offsets written here independently of the code (ADR 0003 and 0005).
const OFF_TYPE: usize = 4;
const OFF_CELL_COUNT: usize = 16;
const OFF_FREE_END: usize = 20;
const OFF_NEXT_LEAF: usize = 24;
const OFF_RESERVED: usize = 28;

const TYPE_LEAF: u8 = 4;
const TYPE_OVERFLOW: u8 = 6;

fn slice(buf: &[u8], at: usize, len: usize) -> anyhow::Result<&[u8]> {
    let end = at.checked_add(len).context("range overflow")?;
    buf.get(at..end)
        .with_context(|| format!("range {at}..{end} past buffer of {}", buf.len()))
}

fn u16_at(buf: &[u8], at: usize) -> anyhow::Result<u16> {
    Ok(u16::from_le_bytes(slice(buf, at, 2)?.try_into()?))
}

fn u32_at(buf: &[u8], at: usize) -> anyhow::Result<u32> {
    Ok(u32::from_le_bytes(slice(buf, at, 4)?.try_into()?))
}

fn temp_db() -> anyhow::Result<(TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("test.db");
    Ok((dir, path))
}

fn new_pager(path: &Path) -> anyhow::Result<Pager> {
    Ok(Pager::create(
        path,
        PagerOptions {
            page_size: PageSize::new(u32::try_from(PAGE_SIZE)?)?,
        },
    )?)
}

fn read(pager: &Pager, id: PageId) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; PAGE_SIZE];
    pager.read_page(id, &mut buf)?;
    Ok(buf)
}

/// Checks the raw bytes of an empty leaf. `read_page` already verified the
/// checksum.
fn check_empty_leaf(buf: &[u8]) -> anyhow::Result<()> {
    ensure!(
        slice(buf, OFF_TYPE, 1)? == [TYPE_LEAF],
        "type byte is not 4"
    );
    ensure!(u16_at(buf, OFF_CELL_COUNT)? == 0, "cell_count is not 0");
    ensure!(
        usize::try_from(u32_at(buf, OFF_FREE_END)?)? == PAGE_SIZE,
        "free_end is not the page size"
    );
    ensure!(u32_at(buf, OFF_NEXT_LEAF)? == 0, "next_leaf is not 0");
    ensure!(u32_at(buf, OFF_RESERVED)? == 0, "reserved bytes are not 0");
    Ok(())
}

/// Allocates a page, fills it with `fill` and writes it.
fn write_new_page(
    pager: &mut Pager,
    fill: impl FnOnce(&mut [u8]) -> anyhow::Result<()>,
) -> anyhow::Result<PageId> {
    let id = pager.allocate_page()?;
    let mut buf = vec![0u8; PAGE_SIZE];
    fill(&mut buf)?;
    pager.write_page(id, &mut buf)?;
    Ok(id)
}

fn open_err(pager: &Pager, root: PageId) -> anyhow::Result<BTreeError> {
    match BTree::open(pager, root) {
        Ok(_) => bail!("open of page {} should fail", root.get()),
        Err(e) => Ok(e),
    }
}

fn ensure_invalid_root(err: &BTreeError, page: PageId, reason: &str) -> anyhow::Result<()> {
    match err {
        BTreeError::InvalidRoot { page: p, reason: r } => {
            ensure!(*p == page, "wrong page: {}", p.get());
            ensure!(*r == reason, "wrong reason: {r}");
            Ok(())
        }
        other => bail!("expected InvalidRoot, got {other:?}"),
    }
}

#[test]
fn create_writes_empty_leaf_at_page_1() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;

    let tree = BTree::create(&mut pager)?;

    ensure!(tree.root() == PageId::new(1), "root is not page 1");
    ensure!(pager.page_count() == 2, "create should allocate one page");
    check_empty_leaf(&read(&pager, tree.root())?)?;

    pager.sync()?;
    drop(pager);

    let reopened = Pager::open(&path)?;
    check_empty_leaf(&read(&reopened, PageId::new(1))?)?;
    Ok(())
}

#[test]
fn two_creates_give_distinct_roots() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;

    let a = BTree::create(&mut pager)?;
    let b = BTree::create(&mut pager)?;

    ensure!(a.root() != b.root(), "roots are equal");
    ensure!(a != b, "trees are equal");
    check_empty_leaf(&read(&pager, a.root())?)?;
    check_empty_leaf(&read(&pager, b.root())?)?;
    Ok(())
}

#[test]
fn create_reuses_a_freed_page() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let a = BTree::create(&mut pager)?;
    let b = BTree::create(&mut pager)?;
    let count = pager.page_count();

    pager.free_page(a.root())?;
    let c = BTree::create(&mut pager)?;

    ensure!(c.root() == a.root(), "freed page was not reused");
    ensure!(c.root() != b.root(), "reused the wrong page");
    ensure!(pager.page_count() == count, "file grew");
    check_empty_leaf(&read(&pager, c.root())?)?;
    Ok(())
}

#[test]
fn open_accepts_created_root() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;

    ensure!(
        BTree::open(&pager, tree.root())? == tree,
        "open differs from create"
    );

    pager.sync()?;
    drop(pager);

    let reopened = Pager::open(&path)?;
    ensure!(
        BTree::open(&reopened, tree.root())? == tree,
        "open after reopen differs"
    );
    Ok(())
}

#[test]
fn open_accepts_internal_node() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let id = write_new_page(&mut pager, |buf| {
        InternalNodeMut::init(buf, PageId::new(7))?;
        Ok(())
    })?;

    let tree = BTree::open(&pager, id)?;

    ensure!(tree.root() == id, "wrong root");
    Ok(())
}

#[test]
fn open_rejects_page_zero() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    BTree::create(&mut pager)?;

    let err = open_err(&pager, PageId::new(0))?;

    ensure_invalid_root(&err, PageId::new(0), "page 0 is the file header")
}

#[test]
fn open_rejects_pages_past_the_end() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    BTree::create(&mut pager)?;
    let count = pager.page_count();

    for id in [count, count + 1, u32::MAX] {
        let err = open_err(&pager, PageId::new(id))?;
        ensure_invalid_root(&err, PageId::new(id), "page is past the end of the file")?;
    }
    Ok(())
}

#[test]
fn open_rejects_a_free_page() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    pager.free_page(tree.root())?;

    let err = open_err(&pager, tree.root())?;

    ensure_invalid_root(&err, tree.root(), "page is not a B+Tree node")
}

#[test]
fn open_rejects_a_plain_slotted_page() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let id = write_new_page(&mut pager, |buf| {
        SlottedPageMut::init(buf)?;
        Ok(())
    })?;

    let err = open_err(&pager, id)?;

    ensure_invalid_root(&err, id, "page is not a B+Tree node")
}

#[test]
fn open_rejects_an_overflow_page() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let id = write_new_page(&mut pager, |buf| {
        SlottedPageMut::init(buf)?;
        let at = buf.get_mut(OFF_TYPE).context("type byte past buffer")?;
        *at = TYPE_OVERFLOW;
        Ok(())
    })?;
    ensure!(
        read(&pager, id)?.get(OFF_TYPE) == Some(&TYPE_OVERFLOW),
        "not overflow typed"
    );

    let err = open_err(&pager, id)?;

    ensure_invalid_root(&err, id, "page is not a B+Tree node")
}

#[test]
fn open_reports_corrupt_leaf_header_as_node_error() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;

    // free_end past the end of the page; write_page stamps a valid checksum.
    let mut buf = read(&pager, tree.root())?;
    let bad = u32::try_from(PAGE_SIZE + 1)?.to_le_bytes();
    buf.get_mut(OFF_FREE_END..OFF_FREE_END + 4)
        .context("free_end past buffer")?
        .copy_from_slice(&bad);
    pager.write_page(tree.root(), &mut buf)?;

    let err = open_err(&pager, tree.root())?;

    match err {
        BTreeError::Node { page, .. } => {
            ensure!(page == tree.root(), "wrong page: {}", page.get());
            Ok(())
        }
        other => bail!("expected Node, got {other:?}"),
    }
}

#[test]
fn handle_is_copy_and_exposes_root() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;

    let copy = tree;

    ensure!(copy == tree, "copy differs");
    ensure!(tree.root() == PageId::new(1), "root changed");
    ensure!(copy.root() == tree.root(), "copy root differs");
    Ok(())
}
