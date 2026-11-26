//! Model tests (Phase 2, step 12c): random operations against a `BTreeMap`.
//!
//! Every operation runs on the tree and on a `BTreeMap` model, and the results
//! must agree: what `put` and `delete` report, `get`, and range scans in both
//! directions (including inverted bounds, which scan nothing). After every
//! mutation `verify_tree` checks the touched tree. A `Reopen` operation syncs,
//! closes the file and opens it again, so the tests also cover what is on disk.
//!
//! - `prop_tree_matches_btreemap`: one tree, through the `BTree` API.
//! - `prop_many_trees_match_their_models`: 2 to 4 trees in one file, through
//!   the `StorageEngine` trait on `BTreeEngine`. `verify_file` checks that the
//!   trees share no page and that no page leaks, also after the keys of one
//!   tree are all deleted.
//! - `many_trees_reach_three_levels`: a fixed sequence, so that the height
//!   does not depend on the random cases.
//!
//! Page size is 512 bytes. A quarter of the keys are 100 bytes long, and the
//! values go from empty to 3000 bytes (overflow chains), so 200 keys make a
//! tree of three levels.

#[path = "support/db.rs"]
mod db;

use std::collections::BTreeMap;
use std::ops::Bound::{self, Excluded, Included, Unbounded};
use std::ops::RangeBounds;
use std::path::Path;

use anyhow::{Context, ensure};
use db::{new_pager, temp_db};
use proptest::prelude::*;
use storage_engine_rs::storage::btree::overflow::read_value;
use storage_engine_rs::storage::btree::{BTree, BTreeEngine, verify_file, verify_tree};
use storage_engine_rs::storage::engine::{Direction, EngineCursor, StorageEngine};
use storage_engine_rs::storage::pager::disk::Pager;

/// Number of distinct keys.
const KEYS: usize = 200;

type Entries = Vec<(Vec<u8>, Vec<u8>)>;
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Debug, Clone)]
enum Op {
    Put {
        key: usize,
        len: usize,
        seed: u8,
    },
    Delete {
        key: usize,
    },
    Get {
        key: usize,
    },
    Range {
        lo: Bound<usize>,
        hi: Bound<usize>,
        dir: Direction,
    },
    Reopen,
}

fn key_index() -> impl Strategy<Value = usize> {
    0..KEYS
}

fn bound() -> impl Strategy<Value = Bound<usize>> {
    prop_oneof![
        Just(Unbounded),
        key_index().prop_map(Included),
        key_index().prop_map(Excluded),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        40 => (
            key_index(),
            prop_oneof![0usize..30, 60usize..110, 130usize..700, 1500usize..3000],
            any::<u8>(),
        )
            .prop_map(|(key, len, seed)| Op::Put { key, len, seed }),
        25 => key_index().prop_map(|key| Op::Delete { key }),
        10 => key_index().prop_map(|key| Op::Get { key }),
        20 => (bound(), bound(), any::<bool>()).prop_map(|(lo, hi, forward)| Op::Range {
            lo,
            hi,
            dir: if forward { Direction::Forward } else { Direction::Reverse },
        }),
        5 => Just(Op::Reopen),
    ]
}

fn model_key(i: usize) -> Vec<u8> {
    let mut key = format!("k{i:04}").into_bytes();
    if i.is_multiple_of(4) {
        key.resize(100, b'.');
    }
    key
}

/// `len` bytes that depend on the position and on `seed`.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| {
            u8::try_from(i.wrapping_mul(31).wrapping_add(usize::from(seed)) % 251).unwrap_or(0)
        })
        .collect()
}

/// The key bounds of a range operation, as owned keys.
fn key_bounds(lo: Bound<usize>, hi: Bound<usize>) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    (lo.map(model_key), hi.map(model_key))
}

/// The entries of the model in the range. Not `BTreeMap::range`: it panics on
/// inverted bounds, and the tree must return an empty scan for them.
fn expected_range(model: &Model, lo: Bound<&[u8]>, hi: Bound<&[u8]>, dir: Direction) -> Entries {
    let bounds = (lo, hi);
    let mut out: Entries = model
        .iter()
        .filter(|(key, _)| RangeBounds::<[u8]>::contains(&bounds, key.as_slice()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if dir == Direction::Reverse {
        out.reverse();
    }
    out
}

/// Scans through the `BTree` API, reading the values in full.
fn scan_tree(
    pager: &Pager,
    tree: BTree,
    lo: Bound<&[u8]>,
    hi: Bound<&[u8]>,
    dir: Direction,
) -> anyhow::Result<Entries> {
    let mut cursor = tree.range(pager, lo, hi, dir)?;
    let mut out = Vec::new();
    while let Some(cell) = cursor.next_entry()? {
        let mut value = Vec::new();
        read_value(pager, cell.value, &mut value)?;
        out.push((cell.key.to_vec(), value));
    }
    Ok(out)
}

/// Scans through the `StorageEngine` trait.
fn scan_engine(
    engine: &BTreeEngine,
    tree: BTree,
    lo: Bound<&[u8]>,
    hi: Bound<&[u8]>,
    dir: Direction,
) -> anyhow::Result<Entries> {
    let mut cursor = engine.scan(tree, lo, hi, dir)?;
    let mut value = Vec::new();
    let mut out = Vec::new();
    while let Some(key) = cursor.next_entry(Some(&mut value))? {
        out.push((key.to_vec(), value.clone()));
    }
    Ok(out)
}

fn ensure_same(got: &Entries, expected: &Entries, what: &str) -> anyhow::Result<()> {
    ensure!(
        got == expected,
        "{what}: the scan returned {} entries, the model {}",
        got.len(),
        expected.len()
    );
    Ok(())
}

fn ensure_get(got: Option<&[u8]>, model: &Model, key: &[u8]) -> anyhow::Result<()> {
    ensure!(
        got == model.get(key).map(Vec::as_slice),
        "get of {key:?} differs from the model"
    );
    Ok(())
}

// ------------------------------------------------------------ one tree

fn run_single(ops: &[Op]) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let mut tree = BTree::create(&mut pager)?;
    let mut model = Model::new();

    for op in ops {
        match op {
            Op::Put { key, len, seed } => {
                let key = model_key(*key);
                let value = pattern(*len, *seed);
                let replaced = tree.put(&mut pager, &key, &value)?;
                ensure!(
                    replaced == model.insert(key.clone(), value).is_some(),
                    "put of {key:?} returned {replaced}"
                );
                verify_tree(&pager, tree)?;
            }
            Op::Delete { key } => {
                let key = model_key(*key);
                let deleted = tree.delete(&mut pager, &key)?;
                ensure!(
                    deleted == model.remove(&key).is_some(),
                    "delete of {key:?} returned {deleted}"
                );
                verify_tree(&pager, tree)?;
            }
            Op::Get { key } => {
                let key = model_key(*key);
                let mut out = Vec::new();
                let found = tree.get(&pager, &key, &mut out)?;
                ensure_get(found.then_some(out.as_slice()), &model, &key)?;
            }
            Op::Range { lo, hi, dir } => {
                let (lo, hi) = key_bounds(*lo, *hi);
                let (lo, hi) = (
                    lo.as_ref().map(Vec::as_slice),
                    hi.as_ref().map(Vec::as_slice),
                );
                ensure_same(
                    &scan_tree(&pager, tree, lo, hi, *dir)?,
                    &expected_range(&model, lo, hi, *dir),
                    "range",
                )?;
            }
            Op::Reopen => {
                pager.sync()?;
                drop(pager);
                pager = Pager::open(&path)?;
                tree = BTree::open(&pager, tree.root())?;
            }
        }
    }

    verify_file(&pager, &[tree])?;
    for dir in [Direction::Forward, Direction::Reverse] {
        ensure_same(
            &scan_tree(&pager, tree, Unbounded, Unbounded, dir)?,
            &expected_range(&model, Unbounded, Unbounded, dir),
            "full scan",
        )?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn prop_tree_matches_btreemap(ops in prop::collection::vec(op(), 1..300)) {
        run_single(&ops).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}

// ----------------------------------------------------------- many trees

fn reopen_engine(
    engine: BTreeEngine,
    path: &Path,
    trees: &[BTree],
) -> anyhow::Result<(BTreeEngine, Vec<BTree>)> {
    engine.sync()?;
    drop(engine.into_pager());
    let engine = BTreeEngine::new(Pager::open(path)?);
    let trees = trees
        .iter()
        .map(|tree| engine.open_tree(tree.root()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((engine, trees))
}

/// Every tree matches its model in a full scan, in both directions.
fn check_all(engine: &BTreeEngine, trees: &[BTree], models: &[Model]) -> anyhow::Result<()> {
    for (&tree, model) in trees.iter().zip(models) {
        for dir in [Direction::Forward, Direction::Reverse] {
            ensure_same(
                &scan_engine(engine, tree, Unbounded, Unbounded, dir)?,
                &expected_range(model, Unbounded, Unbounded, dir),
                "full scan",
            )?;
        }
    }
    Ok(())
}

/// Runs `ops` on `count` trees of one file. Returns the greatest height each
/// tree reached.
fn run_many(count: usize, ops: &[(usize, Op)]) -> anyhow::Result<Vec<usize>> {
    let (_dir, path) = temp_db()?;
    let mut engine = BTreeEngine::new(new_pager(&path)?);
    let mut trees = Vec::new();
    for _ in 0..count {
        trees.push(engine.create_tree()?);
    }
    let mut models = vec![Model::new(); count];
    let mut heights = vec![0usize; count];

    for (step, (which, op)) in ops.iter().enumerate() {
        let which = which.checked_rem(count).context("no trees")?;
        let tree = *trees.get(which).context("no such tree")?;
        let model = models.get_mut(which).context("no such model")?;
        let mut mutated = false;
        match op {
            Op::Put { key, len, seed } => {
                let key = model_key(*key);
                let value = pattern(*len, *seed);
                let replaced = engine.put(tree, &key, &value)?;
                ensure!(
                    replaced == model.insert(key.clone(), value).is_some(),
                    "put of {key:?} returned {replaced}"
                );
                mutated = true;
            }
            Op::Delete { key } => {
                let key = model_key(*key);
                let deleted = engine.delete(tree, &key)?;
                ensure!(
                    deleted == model.remove(&key).is_some(),
                    "delete of {key:?} returned {deleted}"
                );
                mutated = true;
            }
            Op::Get { key } => {
                let key = model_key(*key);
                let mut out = Vec::new();
                let found = engine.get(tree, &key, &mut out)?;
                ensure_get(found.then_some(out.as_slice()), model, &key)?;
            }
            Op::Range { lo, hi, dir } => {
                let (lo, hi) = key_bounds(*lo, *hi);
                let (lo, hi) = (
                    lo.as_ref().map(Vec::as_slice),
                    hi.as_ref().map(Vec::as_slice),
                );
                ensure_same(
                    &scan_engine(&engine, tree, lo, hi, *dir)?,
                    &expected_range(model, lo, hi, *dir),
                    "range",
                )?;
            }
            Op::Reopen => {
                (engine, trees) = reopen_engine(engine, &path, &trees)?;
            }
        }
        if mutated {
            let stats = verify_tree(engine.pager(), tree)?;
            let top = heights.get_mut(which).context("no such height")?;
            *top = (*top).max(stats.height);
        }
        if step.checked_rem(16) == Some(15) {
            verify_file(engine.pager(), &trees)?;
        }
    }

    verify_file(engine.pager(), &trees)?;
    check_all(&engine, &trees, &models)?;

    // Deleting every key of tree 0 must free its pages, not leak them.
    let first = *trees.first().context("no trees")?;
    let first_model = models.first_mut().context("no models")?;
    for key in std::mem::take(first_model).into_keys() {
        ensure!(engine.delete(first, &key)?, "delete of {key:?} missed");
    }
    ensure!(
        !engine.get(first, &model_key(0), &mut Vec::new())?,
        "a deleted key is still there"
    );
    verify_file(engine.pager(), &trees)?;
    check_all(&engine, &trees, &models)?;
    Ok(heights)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn prop_many_trees_match_their_models(
        count in 2usize..=4,
        ops in prop::collection::vec((0usize..4, op()), 1..400),
    ) {
        run_many(count, &ops).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}

#[test]
fn many_trees_reach_three_levels() -> anyhow::Result<()> {
    const TREES: usize = 4;
    let mut state = 987_654usize;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        state >> 33
    };
    let mut ops = Vec::new();
    // Every tree gets every key, with values of all sizes, interleaved.
    for round in 0..KEYS {
        for tree in 0..TREES {
            let len = match next() % 4 {
                0 => next() % 30,
                1 => 60 + next() % 50,
                2 => 130 + next() % 570,
                _ => 1500 + next() % 1500,
            };
            let key = round.wrapping_mul(7).wrapping_add(tree.wrapping_mul(13)) % KEYS;
            let seed = u8::try_from(next() % 256)?;
            ops.push((tree, Op::Put { key, len, seed }));
        }
        if round == KEYS / 2 {
            ops.push((0, Op::Reopen));
        }
    }
    for _ in 0..KEYS {
        ops.push((next() % TREES, Op::Delete { key: next() % KEYS }));
    }

    let heights = run_many(TREES, &ops)?;
    let tall = heights.iter().filter(|&&height| height >= 3).count();
    ensure!(tall >= 3, "only {tall} trees reached 3 levels: {heights:?}");
    Ok(())
}
