//! Writes a small pager file so you can inspect its bytes with `xxd`.
//!
//! Usage:
//!
//! ```text
//! cargo run --example pager_demo -- [PATH] [--page-size 512] [--force]
//! ```
//!
//! `PATH` defaults to `target/pager-demo.db`. The pager refuses to overwrite
//! an existing file, so pass `--force` to delete `PATH` first.
//!
//! The file has five pages:
//!
//! - page 0: the file header (`page_count` 5, `freelist_head` 4, `freelist_count` 2)
//! - page 1: a slotted page with a hole (`cherry` was removed), not compacted
//! - page 2: the same page after `compact()`, so `banana` moved
//! - pages 3 and 4: free pages, linked 4 -> 3 -> none (LIFO)
//!
//! The program prints a table of the pages with their file offsets and the
//! `xxd` commands to dump them. The annotated walkthrough is in
//! `docs/guides/inspect-pager-file.md`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use storage_engine_rs::storage::pager::disk::{Pager, PagerOptions};
use storage_engine_rs::storage::pager::page::{PageType, page_type};
use storage_engine_rs::storage::pager::slotted::{SlottedPage, SlottedPageMut};
use storage_engine_rs::storage::pager::types::{PageId, PageSize};
use storage_engine_rs::storage::wal::log::wal_path;

/// Words inserted in this order, each at its sorted position.
const WORDS: [&str; 3] = ["cherry", "apple", "banana"];
/// Word removed to leave a hole in the cell area.
const REMOVED: &str = "cherry";

/// Command line arguments.
#[derive(Debug, Parser)]
#[command(about = "Write a small pager file to inspect with xxd")]
struct Args {
    /// Where to write the database file.
    #[arg(default_value = "target/pager-demo.db")]
    path: PathBuf,
    /// Page size in bytes: a power of two from 512 to 65536.
    #[arg(long, default_value_t = 512)]
    page_size: u32,
    /// Delete PATH first if it already exists.
    #[arg(long)]
    force: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let page_size = PageSize::new(args.page_size).context("invalid --page-size")?;

    let wal = wal_path(&args.path);
    if args.path.exists() || wal.exists() {
        if !args.force {
            bail!(
                "{} already exists; pass --force to delete it first",
                args.path.display()
            );
        }
        for p in [&args.path, &wal] {
            if p.exists() {
                fs::remove_file(p).with_context(|| format!("cannot delete {}", p.display()))?;
            }
        }
    }

    write_demo(&args.path, page_size)?;
    report(&args.path)
}

/// Creates the file with the five demo pages and syncs it.
fn write_demo(path: &Path, page_size: PageSize) -> Result<()> {
    let mut pager = Pager::create(path, PagerOptions { page_size })?;
    let mut buffer = vec![0u8; page_size.as_usize()];

    // Page 1: fragmented. Page 2: compacted.
    write_words_page(&mut pager, &mut buffer, false)?;
    write_words_page(&mut pager, &mut buffer, true)?;

    // Pages 3 and 4: allocated, then freed in order (LIFO: 4 -> 3 -> none).
    let third = pager.allocate_page()?;
    let fourth = pager.allocate_page()?;
    pager.free_page(third)?;
    pager.free_page(fourth)?;

    pager.sync()?;
    Ok(())
}

/// Allocates a page, fills it with the words, removes one, optionally
/// compacts, and writes it.
fn write_words_page(pager: &mut Pager, buffer: &mut [u8], compact: bool) -> Result<()> {
    let id = pager.allocate_page()?;
    SlottedPageMut::init(buffer)?;
    let mut page = SlottedPageMut::new(buffer)?;

    for word in WORDS {
        let position = find(&page.view(), word)?.unwrap_or_else(|insert_at| insert_at);
        page.insert(position, word.as_bytes())?;
    }

    if let Ok(index) = find(&page.view(), REMOVED)? {
        page.remove(index)?;
    }
    if compact {
        page.compact()?;
    }

    pager.write_page(id, buffer)?;
    Ok(())
}

/// Binary searches `page` for `word`: `Ok(index)` if found, else `Err(insert position)`.
fn find(page: &SlottedPage<'_>, word: &str) -> Result<Result<usize, usize>> {
    Ok(page.search_by(|cell| cell.cmp(word.as_bytes()))?)
}

/// Reopens the file, verifies it, prints the page table and `xxd` commands.
fn report(path: &Path) -> Result<()> {
    let pager = Pager::open(path)?;
    pager.verify_freelist()?;

    let page_size = pager.page_size();
    println!(
        "{}: {} pages of {} bytes, {} free\n",
        path.display(),
        pager.page_count(),
        page_size.get(),
        pager.free_page_count()
    );
    println!("{:<5} {:<12} {:<12} notes", "page", "offset", "type");

    let mut buffer = vec![0u8; page_size.as_usize()];
    for number in 0..pager.page_count() {
        let id = PageId::new(number);
        pager.read_page(id, &mut buffer)?;
        let kind = page_type(&buffer)?;
        let notes = describe(kind, &buffer, number)?;
        println!(
            "{:<5} {:<12} {:<12} {notes}",
            number,
            format!("{:#x}", id.offset(page_size)),
            kind.to_string()
        );
    }

    println!("\nDump the whole file (-a collapses repeated zero lines):");
    println!("  xxd -a {}", path.display());
    println!("Dump one page:");
    for number in 0..pager.page_count() {
        let offset = PageId::new(number).offset(page_size);
        println!(
            "  xxd -s {offset:#x} -l {} {}",
            page_size.get(),
            path.display()
        );
    }
    Ok(())
}

/// Short description of the page-specific fields.
fn describe(kind: PageType, buffer: &[u8], number: u32) -> Result<String> {
    Ok(match kind {
        PageType::Slotted => {
            let page = SlottedPage::new(buffer)?;
            let words: Vec<String> = (0..page.cell_count())
                .map(|index| {
                    page.cell(index)
                        .map(|c| String::from_utf8_lossy(c).into_owned())
                })
                .collect::<Result<_, _>>()?;
            format!(
                "free_end={}, slots={}, fragmented={}, cells={words:?}",
                page.free_end(),
                page.cell_count(),
                page.fragmented_bytes()
            )
        }
        PageType::FileHeader => "file header: see ADR 0003".to_string(),
        PageType::Free => format!("free page {number}, next_free is at offset 16"),
        PageType::BTreeLeaf | PageType::BTreeInternal | PageType::Overflow => {
            format!("{kind} page: see ADR 0005")
        }
    })
}
