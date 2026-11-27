//! Model test for the whole pager stack (step 10): file header, free list and
//! slotted page contents together.
//!
//! A random sequence of operations runs against a real `Pager` on a temporary
//! file and against an in-memory model of the whole database. The model keeps
//! the page count, the free list as a stack (the last element is the head, and
//! element `i` links to element `i - 1`), and for every page its state:
//!
//! - `Fresh`: just allocated by growing the file. The page is all zeros, so
//!   reading it must fail with `Corrupted { source: ChecksumMismatch, .. }`.
//! - `Reused`: popped from the free list. It still holds the old free page
//!   until the caller writes it, so reading it must succeed and decode as a
//!   free page (the `allocate_page` doc).
//! - `Written`: a slotted page the test wrote. Reading it must return the
//!   same bytes, with the same cells in the same order.
//! - `Free`: on the free list. Reading it must succeed, and `decode_free_page`
//!   must return the link the model predicts (or `None` at the bottom).
//!
//! Operations: `Allocate` (the id must be the top of the free stack, or
//! `page_count` when the stack is empty), `Free`, `Write` (a new slotted page
//! with 0 to 8 cells), `InsertCell`, `RemoveCell` and `Compact` (read, modify,
//! write back; an insert the model says does not fit must fail with
//! `PageFull` and write nothing), `Read` of any id below `page_count`
//! (page 0 must decode to the expected file header), `Reopen` (sync, drop,
//! `Pager::open`, then a full scan), `Verify` (`verify_freelist`) and
//! `Misuse` (free or write page 0, ids past the end, buffers of the wrong
//! length: the exact error variant, and the file must not change).
//!
//! Selectors are `proptest::sample::Index` values resolved against the current
//! state. An operation that is impossible in the current state (free with no
//! page in use, insert on a model with no written page) is a no-op, so
//! shrinking stays valid.
//!
//! After every operation `check_cheap` compares `page_count` and
//! `free_page_count`, the file length (`page_count * page_size`) and the file
//! header on disk with the model. On `Reopen` and at the end, `check_full`
//! adds `verify_freelist` and reads every page by its state. Written pages
//! are also checked from the raw bytes (invariants I1 to I4 of ADR 0003, a
//! zeroed free gap), not only through the API.
//!
//! Expected values come from the model, not from the code under test.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use common::{
    add, cells_of, check_compacted, check_free_gap_zeroed, check_invariants, empty_page, sub,
};
use proptest::prelude::*;
use proptest::sample::Index;
use storage_engine_rs::storage::pager::disk::{Pager, PagerOptions};
use storage_engine_rs::storage::pager::error::{PageError, PagerError};
use storage_engine_rs::storage::pager::file_header::FileHeader;
use storage_engine_rs::storage::pager::freelist::decode_free_page;
use storage_engine_rs::storage::pager::slotted::{SLOT_SIZE, SLOTTED_HEADER_SIZE, SlottedPageMut};
use storage_engine_rs::storage::pager::types::{PageId, PageSize};
use storage_engine_rs::storage::wal::log::SyncMode;

/// What the model knows about one page (never page 0).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PageState {
    /// Allocated by growing the file: all zeros, no valid checksum.
    Fresh,
    /// Popped from the free list: still holds the old free page.
    Reused,
    /// A slotted page written by the test.
    Written {
        /// The exact bytes on disk (checksum included).
        bytes: Vec<u8>,
        /// The cells, in logical order.
        cells: Vec<Vec<u8>>,
    },
    /// On the free list.
    Free,
}

/// The expected state of the whole database.
#[derive(Debug)]
struct Model {
    page_size: usize,
    page_count: u32,
    /// The free list as a stack: the last element is the head, element `i`
    /// links to element `i - 1`, and element 0 links to nothing.
    free: Vec<u32>,
    /// State of every page in `1..page_count`.
    pages: BTreeMap<u32, PageState>,
}

impl Model {
    fn new(page_size: u32) -> anyhow::Result<Self> {
        Ok(Self {
            page_size: usize::try_from(page_size)?,
            page_count: 1,
            free: Vec::new(),
            pages: BTreeMap::new(),
        })
    }

    fn expected_header(&self) -> anyhow::Result<FileHeader> {
        Ok(FileHeader {
            page_size: PageSize::new(u32::try_from(self.page_size)?)?,
            page_count: self.page_count,
            freelist_head: self.free.last().map(|&id| PageId::new(id)),
            freelist_count: u32::try_from(self.free.len())?,
            catalog_root: None,
        })
    }

    /// The link a free page must hold: the element below it in the stack.
    fn free_link(&self, id: u32) -> anyhow::Result<Option<PageId>> {
        let at = self
            .free
            .iter()
            .position(|&f| f == id)
            .with_context(|| format!("page {id} is not in the model free list"))?;
        Ok(at
            .checked_sub(1)
            .and_then(|below| self.free.get(below))
            .map(|&below| PageId::new(below)))
    }

    /// Pages that are not free.
    fn in_use(&self) -> Vec<u32> {
        self.pages
            .iter()
            .filter(|(_, state)| !matches!(state, PageState::Free))
            .map(|(&id, _)| id)
            .collect()
    }

    /// Pages that hold a slotted page written by the test.
    fn written(&self) -> Vec<u32> {
        self.pages
            .iter()
            .filter(|(_, state)| matches!(state, PageState::Written { .. }))
            .map(|(&id, _)| id)
            .collect()
    }

    fn cells_of_page(&self, id: u32) -> anyhow::Result<&Vec<Vec<u8>>> {
        match self.pages.get(&id) {
            Some(PageState::Written { cells, .. }) => Ok(cells),
            other => bail!("model: page {id} is not written: {other:?}"),
        }
    }
}

/// Total free bytes of a slotted page of `size` bytes holding `cells`: the
/// page minus the 32-byte header, minus a 4-byte slot and the bytes of each
/// cell (ADR 0003).
fn free_bytes(size: usize, cells: &[Vec<u8>]) -> anyhow::Result<usize> {
    let used = cells
        .iter()
        .try_fold(0usize, |acc, cell| add(acc, add(cell.len(), SLOT_SIZE)?))?;
    sub(sub(size, SLOTTED_HEADER_SIZE)?, used)
}

/// One misuse of the API that must be rejected without any change.
#[derive(Debug, Clone)]
enum Misuse {
    FreeHeader,
    /// `free_page(page_count + n)`.
    FreeBeyond(u32),
    /// `read_page(page_count + n)`.
    ReadBeyond(u32),
    /// `write_page(page_count + n)` with a correct buffer.
    WriteBeyond(u32),
    /// `write_page(0)` with a correct buffer.
    WriteHeader,
    /// `read_page(0)` with a buffer of the wrong length.
    ReadWrongLen(Index),
    /// `write_page(0)` with a buffer of the wrong length.
    WriteWrongLen(Index),
}

#[derive(Debug, Clone)]
enum Op {
    Allocate,
    Free(Index),
    Write(Index, Vec<Vec<u8>>),
    InsertCell(Index, Index, Vec<u8>),
    RemoveCell(Index, Index),
    Compact(Index),
    Read(Index),
    Reopen,
    Verify,
    Misuse(Misuse),
}

fn cell_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..=40)
}

fn misuse_strategy() -> impl Strategy<Value = Misuse> {
    prop_oneof![
        Just(Misuse::FreeHeader),
        (0u32..4).prop_map(Misuse::FreeBeyond),
        (0u32..4).prop_map(Misuse::ReadBeyond),
        (0u32..4).prop_map(Misuse::WriteBeyond),
        Just(Misuse::WriteHeader),
        any::<Index>().prop_map(Misuse::ReadWrongLen),
        any::<Index>().prop_map(Misuse::WriteWrongLen),
    ]
}

/// Operations that change the content of a written page.
fn content_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (any::<Index>(), prop::collection::vec(cell_strategy(), 0..8))
            .prop_map(|(page, cells)| Op::Write(page, cells)),
        2 => (any::<Index>(), any::<Index>(), cell_strategy())
            .prop_map(|(page, at, cell)| Op::InsertCell(page, at, cell)),
        1 => (any::<Index>(), any::<Index>()).prop_map(|(page, at)| Op::RemoveCell(page, at)),
        1 => any::<Index>().prop_map(Op::Compact),
    ]
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => Just(Op::Allocate),
        2 => any::<Index>().prop_map(Op::Free),
        7 => content_strategy(),
        3 => any::<Index>().prop_map(Op::Read),
        1 => Just(Op::Reopen),
        1 => Just(Op::Verify),
        1 => misuse_strategy().prop_map(Op::Misuse),
    ]
}

/// Reads page `id` into a new buffer.
fn read(pager: &Pager, id: u32) -> Result<Vec<u8>, PagerError> {
    let mut buf = vec![0u8; pager.page_size().as_usize()];
    pager.read_page(PageId::new(id), &mut buf)?;
    Ok(buf)
}

/// Checks the file header, the counters and the file length.
fn check_cheap(pager: &Pager, model: &Model, path: &Path) -> anyhow::Result<()> {
    ensure!(
        pager.page_count() == model.page_count,
        "page_count {} != model {}",
        pager.page_count(),
        model.page_count
    );
    ensure!(
        usize::try_from(pager.free_page_count())? == model.free.len(),
        "free_page_count {} != model {}",
        pager.free_page_count(),
        model.free.len()
    );
    let expected_len = u64::from(model.page_count)
        .checked_mul(u64::try_from(model.page_size)?)
        .context("file length overflow")?;
    let len = fs::metadata(path)?.len();
    ensure!(len == expected_len, "file length {len} != {expected_len}");
    let header = FileHeader::decode(&read(pager, 0)?)?;
    ensure!(
        header == model.expected_header()?,
        "file header {header:?} != model {:?}",
        model.expected_header()?
    );
    Ok(())
}

/// Reads page `id` and checks it against its model state.
fn check_page(pager: &Pager, model: &Model, id: u32) -> anyhow::Result<()> {
    if id == 0 {
        let header = FileHeader::decode(&read(pager, 0)?)?;
        ensure!(header == model.expected_header()?, "page 0: header differs");
        return Ok(());
    }
    let state = model
        .pages
        .get(&id)
        .with_context(|| format!("model has no state for page {id}"))?;
    let result = read(pager, id);
    match state {
        PageState::Fresh => ensure!(
            matches!(
                result,
                Err(PagerError::Corrupted {
                    source: PageError::ChecksumMismatch { .. },
                    ..
                })
            ),
            "page {id} (fresh): expected a checksum mismatch, got {result:?}"
        ),
        PageState::Reused => {
            let buf = result.with_context(|| format!("page {id} (reused)"))?;
            decode_free_page(&buf)
                .with_context(|| format!("page {id} (reused) must still decode as free"))?;
        }
        PageState::Written { bytes, cells } => {
            let buf = result.with_context(|| format!("page {id} (written)"))?;
            ensure!(&buf == bytes, "page {id}: bytes differ from the model");
            ensure!(&cells_of(&buf)? == cells, "page {id}: cells differ");
            check_invariants(&buf)?;
            check_free_gap_zeroed(&buf)?;
        }
        PageState::Free => {
            let buf = result.with_context(|| format!("page {id} (free)"))?;
            let link = decode_free_page(&buf)?;
            let expected = model.free_link(id)?;
            ensure!(link == expected, "page {id}: link {link:?} != {expected:?}");
        }
    }
    Ok(())
}

/// `check_cheap`, then `verify_freelist`, then every page by its state.
fn check_full(pager: &Pager, model: &Model, path: &Path) -> anyhow::Result<()> {
    check_cheap(pager, model, path)?;
    pager.verify_freelist().context("verify_freelist")?;
    for id in 1..model.page_count {
        check_page(pager, model, id)?;
    }
    Ok(())
}

/// Builds a slotted page with `cells` appended in order. Cells that do not
/// fit are dropped, and the returned list holds only the cells that did.
fn build_page(size: usize, cells: &[Vec<u8>]) -> anyhow::Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let mut buf = empty_page(size)?;
    let mut kept = Vec::new();
    for cell in cells {
        let at = kept.len();
        match SlottedPageMut::new(&mut buf)?.insert(at, cell) {
            Ok(()) => kept.push(cell.clone()),
            Err(PageError::PageFull { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok((buf, kept))
}

/// Writes `buf` as page `id` and records it in the model. `write_page`
/// stamps the checksum into `buf`, so the buffer afterwards is what is on disk.
fn store(
    pager: &mut Pager,
    model: &mut Model,
    id: u32,
    mut buf: Vec<u8>,
    cells: Vec<Vec<u8>>,
) -> anyhow::Result<()> {
    pager.write_page(PageId::new(id), &mut buf)?;
    model
        .pages
        .insert(id, PageState::Written { bytes: buf, cells });
    Ok(())
}

fn expect_err(
    what: &str,
    result: Result<(), PagerError>,
    matches: impl FnOnce(&PagerError) -> bool,
) -> anyhow::Result<()> {
    match result {
        Err(error) if matches(&error) => Ok(()),
        other => bail!("{what}: unexpected result {other:?}"),
    }
}

fn run_misuse(
    pager: &mut Pager,
    model: &Model,
    path: &Path,
    misuse: &Misuse,
) -> anyhow::Result<()> {
    let before = fs::read(path)?;
    let size = model.page_size;
    let count = model.page_count;
    let beyond = |n: u32| count.checked_add(n).context("page id overflow");
    let wrong_len = |selector: &Index| -> anyhow::Result<usize> {
        let len = selector.index(mul2(size)?);
        Ok(if len == size { add(len, 1)? } else { len })
    };
    match misuse {
        Misuse::FreeHeader => expect_err(
            "free_page(0)",
            pager.free_page(PageId::HEADER),
            |e| matches!(e, PagerError::ReservedPage { page } if *page == PageId::HEADER),
        )?,
        Misuse::FreeBeyond(n) => {
            let id = beyond(*n)?;
            expect_err(
                "free_page past the end",
                pager.free_page(PageId::new(id)),
                |e| {
                    matches!(e, PagerError::PageOutOfBounds { page, page_count }
                    if *page == PageId::new(id) && *page_count == count)
                },
            )?;
        }
        Misuse::ReadBeyond(n) => {
            let id = beyond(*n)?;
            let mut buf = vec![0u8; size];
            expect_err(
                "read_page past the end",
                pager.read_page(PageId::new(id), &mut buf),
                |e| {
                    matches!(e, PagerError::PageOutOfBounds { page, page_count }
                        if *page == PageId::new(id) && *page_count == count)
                },
            )?;
        }
        Misuse::WriteBeyond(n) => {
            let id = beyond(*n)?;
            let mut buf = empty_page(size)?;
            expect_err(
                "write_page past the end",
                pager.write_page(PageId::new(id), &mut buf),
                |e| {
                    matches!(e, PagerError::PageOutOfBounds { page, page_count }
                        if *page == PageId::new(id) && *page_count == count)
                },
            )?;
        }
        Misuse::WriteHeader => {
            let mut buf = empty_page(size)?;
            expect_err(
                "write_page(0)",
                pager.write_page(PageId::HEADER, &mut buf),
                |e| matches!(e, PagerError::ReservedPage { page } if *page == PageId::HEADER),
            )?;
        }
        Misuse::ReadWrongLen(selector) => {
            let len = wrong_len(selector)?;
            let mut buf = vec![0u8; len];
            expect_err(
                "read_page with a wrong buffer",
                pager.read_page(PageId::HEADER, &mut buf),
                |e| {
                    matches!(e, PagerError::BufferSize { expected, actual }
                        if *expected == size && *actual == len)
                },
            )?;
        }
        Misuse::WriteWrongLen(selector) => {
            let len = wrong_len(selector)?;
            let mut buf = vec![0u8; len];
            expect_err(
                "write_page with a wrong buffer",
                pager.write_page(PageId::HEADER, &mut buf),
                |e| {
                    matches!(e, PagerError::BufferSize { expected, actual }
                        if *expected == size && *actual == len)
                },
            )?;
        }
    }
    ensure!(
        fs::read(path)? == before,
        "a rejected call changed the file"
    );
    Ok(())
}

fn mul2(n: usize) -> anyhow::Result<usize> {
    n.checked_mul(2).context("mul overflow")
}

/// Resolves `selector` against `ids`, or `None` when `ids` is empty.
fn pick(ids: &[u32], selector: Index) -> Option<u32> {
    if ids.is_empty() {
        None
    } else {
        ids.get(selector.index(ids.len())).copied()
    }
}

fn apply_allocate(pager: &mut Pager, model: &mut Model) -> anyhow::Result<()> {
    let id = pager.allocate_page()?;
    if let Some(top) = model.free.pop() {
        ensure!(id == PageId::new(top), "allocated {id}, expected {top}");
        model.pages.insert(top, PageState::Reused);
    } else {
        ensure!(
            id == PageId::new(model.page_count),
            "allocated {id}, expected {}",
            model.page_count
        );
        model.pages.insert(model.page_count, PageState::Fresh);
        model.page_count = model.page_count.checked_add(1).context("page_count")?;
    }
    Ok(())
}

/// `InsertCell`: read, insert, write back. When the model says the cell does
/// not fit, the insert must fail with `PageFull` and nothing is written.
fn apply_insert(
    pager: &mut Pager,
    model: &mut Model,
    id: u32,
    at: Index,
    cell: &[u8],
) -> anyhow::Result<()> {
    let mut cells = model.cells_of_page(id)?.clone();
    let position = at.index(add(cells.len(), 1)?);
    let fits = add(cell.len(), SLOT_SIZE)? <= free_bytes(model.page_size, &cells)?;
    let mut buf = read(pager, id)?;
    let outcome = SlottedPageMut::new(&mut buf)?.insert(position, cell);
    if fits {
        outcome.with_context(|| format!("insert into page {id} at {position}"))?;
        cells.insert(position, cell.to_vec());
        store(pager, model, id, buf, cells)?;
    } else {
        ensure!(
            matches!(outcome, Err(PageError::PageFull { .. })),
            "page {id}: expected PageFull, got {outcome:?}"
        );
    }
    Ok(())
}

fn apply_remove(pager: &mut Pager, model: &mut Model, id: u32, at: Index) -> anyhow::Result<()> {
    let mut cells = model.cells_of_page(id)?.clone();
    if cells.is_empty() {
        return Ok(());
    }
    let position = at.index(cells.len());
    let mut buf = read(pager, id)?;
    SlottedPageMut::new(&mut buf)?.remove(position)?;
    cells.remove(position);
    store(pager, model, id, buf, cells)
}

fn apply_compact(pager: &mut Pager, model: &mut Model, id: u32) -> anyhow::Result<()> {
    let cells = model.cells_of_page(id)?.clone();
    let mut buf = read(pager, id)?;
    SlottedPageMut::new(&mut buf)?.compact()?;
    check_compacted(&buf)?;
    store(pager, model, id, buf, cells)
}

/// Applies one operation to the pager and to the model, checking the result
/// of the call against the model.
fn apply(pager: &mut Pager, model: &mut Model, path: &Path, op: &Op) -> anyhow::Result<()> {
    match op {
        Op::Allocate => apply_allocate(pager, model)?,
        Op::Free(selector) => {
            if let Some(id) = pick(&model.in_use(), *selector) {
                pager.free_page(PageId::new(id))?;
                model.free.push(id);
                model.pages.insert(id, PageState::Free);
            }
        }
        Op::Write(selector, cells) => {
            if let Some(id) = pick(&model.in_use(), *selector) {
                let (buf, kept) = build_page(model.page_size, cells)?;
                store(pager, model, id, buf, kept)?;
            }
        }
        Op::InsertCell(page, at, cell) => {
            if let Some(id) = pick(&model.written(), *page) {
                apply_insert(pager, model, id, *at, cell)?;
            }
        }
        Op::RemoveCell(page, at) => {
            if let Some(id) = pick(&model.written(), *page) {
                apply_remove(pager, model, id, *at)?;
            }
        }
        Op::Compact(page) => {
            if let Some(id) = pick(&model.written(), *page) {
                apply_compact(pager, model, id)?;
            }
        }
        Op::Read(selector) => {
            let id = u32::try_from(selector.index(usize::try_from(model.page_count)?))?;
            check_page(pager, model, id)?;
        }
        Op::Reopen => {
            pager.sync()?;
            *pager = Pager::open(path)?;
            pager.set_sync_mode(SyncMode::Off);
            check_full(pager, model, path)?;
        }
        Op::Verify => pager.verify_freelist()?,
        Op::Misuse(misuse) => run_misuse(pager, model, path, misuse)?,
    }
    Ok(())
}

fn run(page_size: u32, ops: &[Op]) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("model.db");
    let options = PagerOptions {
        page_size: PageSize::new(page_size)?,
    };
    let mut pager = Pager::create(&path, options)?;
    pager.set_sync_mode(SyncMode::Off);
    let mut model = Model::new(page_size)?;
    check_cheap(&pager, &model, &path)?;
    for (i, op) in ops.iter().enumerate() {
        apply(&mut pager, &mut model, &path, op).with_context(|| format!("op {i}: {op:?}"))?;
        check_cheap(&pager, &model, &path).with_context(|| format!("after op {i}: {op:?}"))?;
    }
    check_full(&pager, &model, &path).context("final check")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn prop_pager_matches_model(
        page_size in prop::sample::select(vec![512u32, 4096]),
        ops in prop::collection::vec(op_strategy(), 1..=60)
    ) {
        run(page_size, &ops).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
