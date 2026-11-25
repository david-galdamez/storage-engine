//! Tests for `BTreeEngine`, the `StorageEngine` implementation (Phase 2,
//! step 10b). Page size is 512 bytes.
//!
//! The tests check:
//!
//! - a generic contract check over the `StorageEngine` trait: `put` reports
//!   replacement, `get` clears `out` first, `delete` reports presence, trees
//!   are independent, and empty keys and values work;
//! - the same check covers `scan`: both directions, every bound kind, empty
//!   and inverted ranges, key-only scans, the value buffer, and fusing;
//! - a large value round-trips through an overflow chain;
//! - a key over `max_key_len` is rejected and nothing is written;
//! - three trees with interleaved puts keep all their keys across splits;
//! - after `sync`, `into_pager` and a reopen, `open_tree` finds the data, and
//!   it rejects page 0 and a page past the end.

use anyhow::ensure;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use storage_engine_rs::storage::btree::error::BTreeError;
use storage_engine_rs::storage::btree::{BTree, BTreeEngine};
use storage_engine_rs::storage::engine::{Direction, EngineCursor, StorageEngine};
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

type Range<'a> = (Bound<&'a [u8]>, Bound<&'a [u8]>);
type Entries = Vec<(Vec<u8>, Vec<u8>)>;

/// Scans with values and returns the entries. Checks the buffer contract:
/// junk is replaced, and the buffer is empty after the end, which then
/// repeats.
fn collect<E: StorageEngine>(
    e: &E,
    tree: E::Tree,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
    dir: Direction,
) -> anyhow::Result<Entries> {
    let mut cursor = e.scan(tree, lower, upper, dir)?;
    let mut buffer = b"junk".to_vec();
    let mut entries = Vec::new();
    while let Some(key) = cursor.next_entry(Some(&mut buffer))? {
        entries.push((key.to_vec(), buffer.clone()));
    }
    ensure!(buffer.is_empty(), "buffer must be empty after the end");
    for _ in 0..2 {
        ensure!(cursor.next_entry(Some(&mut buffer))?.is_none(), "fused");
    }
    Ok(entries)
}

/// Scans without values and returns the keys.
fn collect_keys<E: StorageEngine>(
    e: &E,
    tree: E::Tree,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
    dir: Direction,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut cursor = e.scan(tree, lower, upper, dir)?;
    let mut keys = Vec::new();
    while let Some(key) = cursor.next_entry(None)? {
        keys.push(key.to_vec());
    }
    ensure!(cursor.next_entry(None)?.is_none(), "fused");
    Ok(keys)
}

fn check_scan<E: StorageEngine>(e: &mut E) -> anyhow::Result<()> {
    use Bound::{Excluded, Included, Unbounded};
    let a = e.create_tree()?;
    let b = e.create_tree()?;
    let mut expected: Entries = Vec::new();
    for i in 0..300u32 {
        let key = format!("key-{i:04}").into_bytes();
        let value = format!("value-{i}").into_bytes();
        e.put(a, &key, &value)?;
        expected.push((key, value));
    }
    e.put(b, b"only-in-b", b"x")?;

    let lo = b"key-0100".as_slice();
    let hi = b"key-0200".as_slice();
    let cases: [(Range<'_>, usize, usize); 6] = [
        ((Unbounded, Unbounded), 0, 300),
        ((Included(lo), Excluded(hi)), 100, 200),
        ((Excluded(lo), Included(hi)), 101, 201),
        ((Included(lo), Included(hi)), 100, 201),
        ((Unbounded, Excluded(lo)), 0, 100),
        ((Included(hi), Unbounded), 200, 300),
    ];
    for ((lower, upper), from, to) in cases {
        let want = expected.get(from..to).unwrap_or_default();
        let forward = collect(e, a, lower, upper, Direction::Forward)?;
        ensure!(forward == want, "forward {from}..{to}");
        let mut reverse = collect(e, a, lower, upper, Direction::Reverse)?;
        reverse.reverse();
        ensure!(reverse == want, "reverse {from}..{to}");
        let keys: Vec<Vec<u8>> = want.iter().map(|(k, _)| k.clone()).collect();
        ensure!(collect_keys(e, a, lower, upper, Direction::Forward)? == keys);
        let mut rkeys = collect_keys(e, a, lower, upper, Direction::Reverse)?;
        rkeys.reverse();
        ensure!(rkeys == keys, "key-only reverse {from}..{to}");
    }

    // Empty and inverted ranges are not errors.
    for dir in [Direction::Forward, Direction::Reverse] {
        ensure!(collect(e, a, Included(lo), Excluded(lo), dir)?.is_empty());
        ensure!(collect(e, a, Included(hi), Included(lo), dir)?.is_empty());
        ensure!(collect(e, a, Included(b"zzz"), Unbounded, dir)?.is_empty());
    }

    // Trees are scanned independently.
    let in_b = collect(e, b, Unbounded, Unbounded, Direction::Forward)?;
    ensure!(in_b == [(b"only-in-b".to_vec(), b"x".to_vec())]);
    let empty = e.create_tree()?;
    ensure!(collect(e, empty, Unbounded, Unbounded, Direction::Reverse)?.is_empty());
    Ok(())
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
    check_scan(e)
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

fn pattern(len: usize) -> anyhow::Result<Vec<u8>> {
    Ok((0..len)
        .map(|i| u8::try_from(i % 251))
        .collect::<Result<_, _>>()?)
}

#[test]
fn scan_reads_overflow_values_only_when_asked() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut engine = BTreeEngine::new(new_pager(&path)?);
    let tree = engine.create_tree()?;
    let big = pattern(1500)?;
    engine.put(tree, b"a", b"small")?;
    engine.put(tree, b"b", &big)?;
    engine.put(tree, b"c", b"tail")?;

    let all = collect(
        &engine,
        tree,
        Bound::Unbounded,
        Bound::Unbounded,
        Direction::Forward,
    )?;
    ensure!(all.len() == 3);
    ensure!(all.get(1) == Some(&(b"b".to_vec(), big.clone())));
    let rev = collect(
        &engine,
        tree,
        Bound::Unbounded,
        Bound::Unbounded,
        Direction::Reverse,
    )?;
    ensure!(rev.get(1) == Some(&(b"b".to_vec(), big)));
    Ok(())
}

/// A tree with a value in an overflow chain, whose chain pages are then
/// blanked.
fn engine_with_corrupt_chain(path: &Path) -> anyhow::Result<(BTreeEngine, BTree)> {
    let mut engine = BTreeEngine::new(new_pager(path)?);
    let tree = engine.create_tree()?;
    engine.put(tree, b"a", b"small")?;
    engine.put(tree, b"b", &pattern(1500)?)?;
    engine.put(tree, b"c", b"tail")?;
    let root = tree.root();
    let mut pager = engine.into_pager();
    for id in 1..pager.page_count() {
        if id != root.get() {
            let mut blank = vec![0u8; usize::try_from(pager.page_size().get())?];
            pager.write_page(PageId::new(id), &mut blank)?;
        }
    }
    let engine = BTreeEngine::new(pager);
    let tree = engine.open_tree(root)?;
    Ok((engine, tree))
}

#[test]
fn scan_without_values_ignores_a_corrupt_chain() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let (engine, tree) = engine_with_corrupt_chain(&path)?;
    let keys = collect_keys(
        &engine,
        tree,
        Bound::Unbounded,
        Bound::Unbounded,
        Direction::Forward,
    )?;
    ensure!(keys == [b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    Ok(())
}

#[test]
fn scan_with_values_stops_at_a_corrupt_chain() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let (engine, tree) = engine_with_corrupt_chain(&path)?;
    let mut cursor = engine.scan(tree, Bound::Unbounded, Bound::Unbounded, Direction::Forward)?;
    let mut buffer = b"junk".to_vec();
    ensure!(cursor.next_entry(Some(&mut buffer))? == Some(b"a".as_slice()));
    ensure!(buffer == b"small");
    ensure!(
        cursor.next_entry(Some(&mut buffer)).is_err(),
        "corrupt chain"
    );
    ensure!(buffer.is_empty(), "buffer must be empty after an error");
    for _ in 0..2 {
        ensure!(cursor.next_entry(Some(&mut buffer))?.is_none(), "fused");
        ensure!(buffer.is_empty());
    }
    Ok(())
}

#[derive(Debug, Clone)]
enum Op {
    Put(u8, Vec<u8>),
    Delete(u8),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (any::<u8>(), proptest::collection::vec(any::<u8>(), 0..700))
            .prop_map(|(k, v)| Op::Put(k, v)),
        1 => any::<u8>().prop_map(Op::Delete),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn scan_matches_a_btreemap(
        ops in proptest::collection::vec(op_strategy(), 1..150),
        lower in any::<u8>(),
        upper in any::<u8>(),
    ) {
        let outcome = run_model(&ops, lower, upper);
        prop_assert!(outcome.is_ok(), "{outcome:?}");
    }
}

fn run_model(ops: &[Op], lower: u8, upper: u8) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut engine = BTreeEngine::new(new_pager(&path)?);
    let tree = engine.create_tree()?;
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for op in ops {
        match op {
            Op::Put(k, v) => {
                engine.put(tree, &[*k], v)?;
                model.insert(vec![*k], v.clone());
            }
            Op::Delete(k) => {
                engine.delete(tree, &[*k])?;
                model.remove(&vec![*k]);
            }
        }
    }
    let (lo, hi) = (vec![lower], vec![upper]);
    let ranges: [Range<'_>; 2] = [
        (Bound::Unbounded, Bound::Unbounded),
        (Bound::Included(&lo), Bound::Excluded(&hi)),
    ];
    for (l, u) in ranges {
        let want: Entries = if lower <= upper || matches!(l, Bound::Unbounded) {
            model
                .iter()
                .filter(|(k, _)| {
                    let k = k.as_slice();
                    let above = match l {
                        Bound::Included(b) => k >= b,
                        Bound::Excluded(b) => k > b,
                        Bound::Unbounded => true,
                    };
                    let below = match u {
                        Bound::Included(b) => k <= b,
                        Bound::Excluded(b) => k < b,
                        Bound::Unbounded => true,
                    };
                    above && below
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let keys: Vec<Vec<u8>> = want.iter().map(|(k, _)| k.clone()).collect();
        ensure!(
            collect(&engine, tree, l, u, Direction::Forward)? == want,
            "forward"
        );
        ensure!(collect_keys(&engine, tree, l, u, Direction::Forward)? == keys);
        let mut rev = collect(&engine, tree, l, u, Direction::Reverse)?;
        rev.reverse();
        ensure!(rev == want, "reverse");
        let mut rkeys = collect_keys(&engine, tree, l, u, Direction::Reverse)?;
        rkeys.reverse();
        ensure!(rkeys == keys, "reverse keys");
    }
    Ok(())
}
