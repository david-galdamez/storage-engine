//! Tests for `BTreeEngine`, the `StorageEngine` implementation (Phase 2,
//! step 10b). Page size is 512 bytes.
//!
//! The tests check:
//!
//! - a generic contract check over the `StorageEngine` trait: `put` reports
//!   replacement, `get` clears `out` first, `delete` reports presence, trees
//!   are independent, and empty keys and values work;
//! - a large value round-trips through an overflow chain;
//! - a key over `max_key_len` is rejected and nothing is written;
//! - three trees with interleaved puts keep all their keys across splits;
//! - after `sync`, `into_pager` and a reopen, `open_tree` finds the data, and
//!   it rejects page 0 and a page past the end.

use anyhow::ensure;
use std::path::{Path, PathBuf};

use storage_engine_rs::storage::btree::BTreeEngine;
use storage_engine_rs::storage::btree::error::BTreeError;
use storage_engine_rs::storage::engine::StorageEngine;
use storage_engine_rs::storage::pager::disk::{Pager, PagerOptions};
use storage_engine_rs::storage::pager::types::{PageId, PageSize};
use tempfile::TempDir;

// `btree_support` is not used here: a test binary must use every item of a
// module it includes, and this file needs only these two helpers.
fn temp_db() -> anyhow::Result<(TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("test.db");
    Ok((dir, path))
}

fn new_pager(path: &Path) -> anyhow::Result<Pager> {
    Ok(Pager::create(
        path,
        PagerOptions {
            page_size: PageSize::new(512)?,
        },
    )?)
}

fn lookup<E: StorageEngine>(e: &E, tree: E::Tree, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    Ok(e.get(tree, key, &mut out)?.then_some(out))
}

fn check_engine<E: StorageEngine>(e: &mut E) -> anyhow::Result<()> {
    let a = e.create_tree()?;
    let b = e.create_tree()?;

    ensure!(!e.put(a, b"k", b"v1")?, "new key must report false");
    ensure!(e.put(a, b"k", b"v2")?, "existing key must report true");

    let mut out = b"junk".to_vec();
    ensure!(e.get(a, b"k", &mut out)?, "key must exist");
    ensure!(out == b"v2", "get must replace junk, got {out:?}");
    ensure!(!e.get(a, b"missing", &mut out)?, "missing key must miss");
    ensure!(out.is_empty(), "a miss must clear out, got {out:?}");

    ensure!(lookup(e, b, b"k")?.is_none(), "trees must be independent");
    e.put(b, b"other", b"x")?;
    ensure!(
        lookup(e, a, b"other")?.is_none(),
        "trees must be independent"
    );

    ensure!(e.delete(a, b"k")?, "delete of a present key is true");
    ensure!(!e.delete(a, b"k")?, "second delete is false");
    ensure!(lookup(e, a, b"k")?.is_none(), "deleted key must miss");

    ensure!(!e.put(a, b"", b"")?, "empty key and value must insert");
    ensure!(lookup(e, a, b"")?.as_deref() == Some(b"".as_slice()));
    Ok(())
}

#[test]
fn engine_satisfies_the_trait_contract() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut engine = BTreeEngine::new(new_pager(&path)?);
    check_engine(&mut engine)
}

#[test]
fn large_value_round_trips_through_overflow() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut engine = BTreeEngine::new(new_pager(&path)?);
    let tree = engine.create_tree()?;
    let value: Vec<u8> = (0..5000u32)
        .map(|i| u8::try_from(i % 251))
        .collect::<Result<_, _>>()?;
    ensure!(!engine.put(tree, b"big", &value)?);
    ensure!(lookup(&engine, tree, b"big")? == Some(value));
    Ok(())
}

#[test]
fn key_too_large_is_rejected_and_nothing_is_written() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut engine = BTreeEngine::new(new_pager(&path)?);
    let tree = engine.create_tree()?;
    let key = vec![b'k'; 4096];
    let pages_before = engine.pager().page_count();
    match engine.put(tree, &key, b"v") {
        Err(BTreeError::KeyTooLarge { len, .. }) => ensure!(len == key.len()),
        other => anyhow::bail!("expected KeyTooLarge, got {other:?}"),
    }
    ensure!(
        engine.pager().page_count() == pages_before,
        "no page allocated"
    );
    ensure!(lookup(&engine, tree, &key)?.is_none());
    Ok(())
}

#[test]
fn three_trees_with_interleaved_puts_keep_all_keys() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut engine = BTreeEngine::new(new_pager(&path)?);
    let trees = [
        engine.create_tree()?,
        engine.create_tree()?,
        engine.create_tree()?,
    ];
    for i in 0..500u32 {
        for (n, tree) in trees.iter().enumerate() {
            let key = format!("key-{i:05}");
            let value = format!("tree-{n}-value-{i}");
            ensure!(!engine.put(*tree, key.as_bytes(), value.as_bytes())?);
        }
    }
    for i in 0..500u32 {
        for (n, tree) in trees.iter().enumerate() {
            let key = format!("key-{i:05}");
            let expected = format!("tree-{n}-value-{i}");
            ensure!(lookup(&engine, *tree, key.as_bytes())? == Some(expected.into_bytes()));
        }
    }
    Ok(())
}

#[test]
fn data_survives_sync_and_reopen() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut engine = BTreeEngine::new(new_pager(&path)?);
    let tree = engine.create_tree()?;
    for i in 0..200u32 {
        let key = format!("key-{i:05}");
        engine.put(tree, key.as_bytes(), format!("v{i}").as_bytes())?;
    }
    let root = tree.root();
    engine.sync()?;
    drop(engine.into_pager());

    let engine = BTreeEngine::new(Pager::open(&path)?);
    let tree = engine.open_tree(root)?;
    for i in 0..200u32 {
        let key = format!("key-{i:05}");
        let got = lookup(&engine, tree, key.as_bytes())?;
        ensure!(got == Some(format!("v{i}").into_bytes()), "key {i}");
    }

    let zero = PageId::new(0);
    ensure!(matches!(
        engine.open_tree(zero),
        Err(BTreeError::InvalidRoot { .. })
    ));
    let past = PageId::new(engine.pager().page_count());
    ensure!(matches!(
        engine.open_tree(past),
        Err(BTreeError::InvalidRoot { .. })
    ));
    Ok(())
}
