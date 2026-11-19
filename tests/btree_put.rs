//! Tests for `BTree::put` (Phase 2, step 5): insert and upsert.
//!
//! The split cases are in `btree_split.rs`; here a full leaf is split and
//! every key is kept. Multi-level trees are built by hand with the helpers in
//! `btree_support`, with leaves that have a little free room. Page size is 512
//! bytes.
//!
//! The tests check:
//!
//! - a put into an empty tree, and replaces with larger, smaller and empty
//!   values (the cell count does not change);
//! - the empty key, the largest inline value and the largest key, and the
//!   errors one byte past each limit (`Unsupported`, `KeyTooLarge`), with the
//!   file unchanged;
//! - keys inserted in random order end up in ascending order;
//! - a put into a full leaf splits it and every key is kept;
//! - many replaces of one key reclaim fragmented space (compaction);
//! - a put into a three-level tree routes the key to the right leaf;
//! - replacing an overflow value gives `Unsupported`, nothing is written;
//! - `put` survives closing and reopening the file;
//! - a property test against a `BTreeMap` model.

mod btree_support;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use btree_support::{
    blank, bulk_load, ensure_missing, ensure_value, get, height, new_pager, numbered_entries,
    page_size, temp_db, write_leaf,
};
use proptest::prelude::*;
use storage_engine_rs::storage::btree::BTree;
use storage_engine_rs::storage::btree::error::BTreeError;
use storage_engine_rs::storage::btree::internal::InternalNode;
use storage_engine_rs::storage::btree::leaf::{LeafNode, LeafNodeMut};
use storage_engine_rs::storage::btree::node::{ValueRef, max_key_len, max_node_cell_len};
use storage_engine_rs::storage::pager::disk::Pager;
use storage_engine_rs::storage::pager::types::PageId;

fn read(pager: &Pager, id: PageId) -> anyhow::Result<Vec<u8>> {
    let mut buf = blank();
    pager.read_page(id, &mut buf)?;
    Ok(buf)
}

/// The cells of the leaf at `id` as `(key, value)` pairs, after `validate`.
fn leaf_entries(pager: &Pager, id: PageId) -> anyhow::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let buf = read(pager, id)?;
    let leaf = LeafNode::new(&buf)?;
    leaf.validate()?;
    let mut entries = Vec::new();
    for index in 0..leaf.cell_count() {
        let cell = leaf.cell(index)?;
        match cell.value {
            ValueRef::Inline(value) => entries.push((cell.key.to_vec(), value.to_vec())),
            ValueRef::Overflow { .. } => bail!("unexpected overflow cell"),
        }
    }
    Ok(entries)
}

/// The leaf a key is routed to, by walking the internal nodes by hand.
fn leaf_of(pager: &Pager, root: PageId, key: &[u8]) -> anyhow::Result<PageId> {
    let mut id = root;
    loop {
        let buf = read(pager, id)?;
        match InternalNode::new(&buf) {
            Ok(node) => id = node.child_for(key)?.1,
            Err(_) => return Ok(id),
        }
    }
}

fn file_bytes(path: &Path) -> anyhow::Result<Vec<u8>> {
    Ok(std::fs::read(path)?)
}

fn put_err(tree: BTree, pager: &mut Pager, key: &[u8], value: &[u8]) -> anyhow::Result<BTreeError> {
    match tree.put(pager, key, value) {
        Ok(replaced) => bail!("put should fail, returned {replaced}"),
        Err(e) => Ok(e),
    }
}

fn ensure_unsupported(err: &BTreeError, prefix: &str) -> anyhow::Result<()> {
    match err {
        BTreeError::Unsupported { what } => {
            ensure!(what.starts_with(prefix), "what: {what}");
            Ok(())
        }
        other => bail!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn put_into_an_empty_tree() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;

    ensure!(
        !tree.put(&mut pager, b"a", b"1")?,
        "new key reported replace"
    );

    ensure_value(get(tree, &pager, b"a")?.as_deref(), b"1", "a")?;
    ensure_missing(get(tree, &pager, b"b")?.as_deref(), "b")?;
    ensure!(
        leaf_entries(&pager, tree.root())? == vec![(b"a".to_vec(), b"1".to_vec())],
        "leaf contents"
    );
    Ok(())
}

#[test]
fn replace_with_larger_smaller_and_empty_values() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    for key in [&b"a"[..], b"b", b"c"] {
        tree.put(&mut pager, key, b"mid")?;
    }

    for (what, value) in [
        ("larger", &b"a much larger value"[..]),
        ("smaller", b"x"),
        ("empty", b""),
        ("again", b"back"),
    ] {
        ensure!(tree.put(&mut pager, b"b", value)?, "{what}: not a replace");
        ensure_value(get(tree, &pager, b"b")?.as_deref(), value, what)?;
        ensure!(
            leaf_entries(&pager, tree.root())?.len() == 3,
            "{what}: count"
        );
    }
    ensure_value(get(tree, &pager, b"a")?.as_deref(), b"mid", "a untouched")?;
    ensure_value(get(tree, &pager, b"c")?.as_deref(), b"mid", "c untouched")
}

#[test]
fn empty_key_works() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;

    ensure!(!tree.put(&mut pager, b"z", b"z")?, "z");
    ensure!(!tree.put(&mut pager, b"", b"root")?, "empty key is new");
    ensure!(tree.put(&mut pager, b"", b"root2")?, "empty key replace");

    ensure_value(get(tree, &pager, b"")?.as_deref(), b"root2", "empty key")?;
    let entries = leaf_entries(&pager, tree.root())?;
    ensure!(
        entries.first().map(|e| e.0.as_slice()) == Some(&b""[..]),
        "empty key sorts first"
    );
    Ok(())
}

#[test]
fn value_at_the_inline_maximum_and_one_more() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let key = b"key";
    let max_value = max_node_cell_len(page_size()?)
        .checked_sub(3)
        .and_then(|n| n.checked_sub(key.len()))
        .context("limit underflow")?;
    let value = vec![0xAB; max_value];

    ensure!(!tree.put(&mut pager, key, &value)?, "max inline put");
    ensure_value(get(tree, &pager, key)?.as_deref(), &value, "max inline")?;

    let before = file_bytes(&path)?;
    let too_big = vec![0xCD; max_value.checked_add(1).context("overflow")?];
    let err = put_err(tree, &mut pager, key, &too_big)?;
    ensure_unsupported(&err, "overflow values")?;
    let err = put_err(tree, &mut pager, b"other", &too_big)?;
    ensure_unsupported(&err, "overflow values")?;
    ensure!(file_bytes(&path)? == before, "file changed by a failed put");
    Ok(())
}

#[test]
fn key_at_the_maximum_and_one_more() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let max = max_key_len(page_size()?);
    let key = vec![b'k'; max];

    ensure!(!tree.put(&mut pager, &key, b"v")?, "max key put");
    ensure_value(get(tree, &pager, &key)?.as_deref(), b"v", "max key")?;

    let before = file_bytes(&path)?;
    let too_long = vec![b'k'; max.checked_add(1).context("overflow")?];
    match put_err(tree, &mut pager, &too_long, b"v")? {
        BTreeError::KeyTooLarge { len, max: limit } => {
            ensure!(len == too_long.len(), "len: {len}");
            ensure!(limit == max, "max: {limit}");
        }
        other => bail!("expected KeyTooLarge, got {other:?}"),
    }
    ensure!(file_bytes(&path)? == before, "file changed by a failed put");
    Ok(())
}

#[test]
fn random_order_inserts_end_up_sorted() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    // 7 is coprime with 13, so this visits 0..13 in a scrambled order.
    let order: Vec<usize> = (0..13usize).map(|i| i.saturating_mul(7) % 13).collect();
    ensure!(
        order.windows(2).any(|w| w.first() > w.get(1)),
        "order is sorted"
    );

    for i in &order {
        let key = format!("k{i:02}");
        ensure!(
            !tree.put(&mut pager, key.as_bytes(), key.as_bytes())?,
            "new key {key}"
        );
    }

    let entries = leaf_entries(&pager, tree.root())?;
    let keys: Vec<&[u8]> = entries.iter().map(|e| e.0.as_slice()).collect();
    ensure!(keys.len() == 13, "count: {}", keys.len());
    ensure!(
        keys.windows(2).all(|w| w.first() < w.get(1)),
        "keys not ascending"
    );
    Ok(())
}

#[test]
fn full_leaf_is_split_and_every_key_is_kept() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let value = [7u8; 20];

    let mut inserted = Vec::new();
    for i in 0..100usize {
        let key = format!("k{i:03}").into_bytes();
        ensure!(
            !tree.put(&mut pager, &key, &value)?,
            "new key reported replace"
        );
        inserted.push(key);

        // The first split turns the root leaf into an internal node.
        if InternalNode::new(&read(&pager, tree.root())?).is_ok() {
            ensure!(
                inserted.len() > 5,
                "leaf split too early: {}",
                inserted.len()
            );
            for key in &inserted {
                ensure_value(get(tree, &pager, key)?.as_deref(), &value, "kept key")?;
            }
            return Ok(());
        }
    }
    bail!("the leaf never split")
}

#[test]
fn many_replaces_reclaim_fragmented_space() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    // 8 cells of 37 bytes + slots use 328 of 480 usable bytes. Replacing one
    // of them with sizes between 5 and 60 bytes leaves holes that only a
    // compaction can reuse.
    for i in 0..8usize {
        let key = format!("k{i:03}");
        tree.put(&mut pager, key.as_bytes(), &[1u8; 30])?;
    }

    let mut last = Vec::new();
    for round in 0..300usize {
        let len = if round % 2 == 0 { 60 } else { 5 };
        let fill = u8::try_from(round % 251)?;
        last = vec![fill; len];
        ensure!(
            tree.put(&mut pager, b"k003", &last)?,
            "round {round}: not a replace"
        );
        ensure_value(
            get(tree, &pager, b"k003")?.as_deref(),
            &last,
            "latest value",
        )?;
    }

    let entries = leaf_entries(&pager, tree.root())?;
    ensure!(entries.len() == 8, "count: {}", entries.len());
    ensure!(
        entries
            .iter()
            .filter(|e| e.0 == b"k003")
            .all(|e| e.1 == last),
        "latest value on disk"
    );
    Ok(())
}

#[test]
fn put_routes_keys_in_a_three_level_tree() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    // Keys "key-00000", "key-00002", ... with 88-byte values: cells of 100
    // bytes, 4 per leaf, so every leaf has 64 bytes free (room for several
    // new keys with empty values).
    let entries: Vec<(Vec<u8>, Vec<u8>)> = numbered_entries(100)
        .into_iter()
        .map(|(key, _)| (key, vec![0x11; 88]))
        .collect();
    let root = bulk_load(&mut pager, &entries)?;
    ensure!(height(&pager, root)? >= 3, "tree is too shallow");
    let tree = BTree::open(&pager, root)?;

    let new_keys: [&[u8]; 5] = [
        b"a",         // below the first separator
        b"key-00001", // inside the first leaf
        b"key-00101", // between separators
        b"key-00199", // inside the last leaf
        b"zzz",       // above the last key
    ];
    for key in new_keys {
        let leaf = leaf_of(&pager, root, key)?;
        ensure!(
            !tree.put(&mut pager, key, b"")?,
            "{key:?}: new key reported replace"
        );
        let in_leaf = leaf_entries(&pager, leaf)?;
        ensure!(
            in_leaf.iter().any(|e| e.0 == key && e.1.is_empty()),
            "{key:?} not in the leaf the descent chose"
        );
        ensure!(
            leaf_of(&pager, root, key)? == leaf,
            "{key:?} routes to another leaf after the put"
        );
    }

    // Keys equal to separators (the first key of each leaf) are replaced in
    // place.
    let mut replaced = BTreeMap::new();
    for (position, (key, _)) in entries.iter().enumerate().step_by(4) {
        let value = vec![0x22; 10];
        ensure!(
            tree.put(&mut pager, key, &value)?,
            "separator {position}: not a replace"
        );
        replaced.insert(key.clone(), value);
    }

    for (key, value) in &entries {
        let expected = replaced.get(key).unwrap_or(value);
        ensure_value(get(tree, &pager, key)?.as_deref(), expected, "original key")?;
    }
    for key in new_keys {
        ensure_value(get(tree, &pager, key)?.as_deref(), b"", "new key")?;
    }
    Ok(())
}

#[test]
fn replacing_an_overflow_value_is_unsupported() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let id = write_leaf(&mut pager, &[(b"a", b"A")], None)?;
    let mut buf = read(&pager, id)?;
    LeafNodeMut::new(&mut buf)?.insert(
        1,
        b"big",
        ValueRef::Overflow {
            len: 5000,
            first_page: PageId::new(99),
        },
    )?;
    pager.write_page(id, &mut buf)?;
    let tree = BTree::open(&pager, id)?;
    let before = file_bytes(&path)?;

    let err = put_err(tree, &mut pager, b"big", b"small")?;
    ensure_unsupported(&err, "replacing overflow")?;
    ensure!(file_bytes(&path)? == before, "file changed by a failed put");

    // The other cells still accept puts.
    ensure!(tree.put(&mut pager, b"a", b"A2")?, "replace a");
    ensure!(!tree.put(&mut pager, b"b", b"B")?, "insert b");
    Ok(())
}

#[test]
fn put_survives_reopening_the_file() -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let root = tree.root();
    let entries = numbered_entries(10);
    for (key, value) in &entries {
        tree.put(&mut pager, key, value)?;
    }
    tree.put(
        &mut pager,
        &entries.first().context("entries")?.0,
        b"changed",
    )?;
    pager.sync()?;
    drop(pager);

    let reopened = Pager::open(&path)?;
    let tree = BTree::open(&reopened, root)?;
    for (index, (key, value)) in entries.iter().enumerate() {
        let expected: &[u8] = if index == 0 { b"changed" } else { value };
        ensure_value(
            get(tree, &reopened, key)?.as_deref(),
            expected,
            "after reopen",
        )?;
    }
    Ok(())
}

fn run_model(keys: &[Vec<u8>], ops: &[(usize, Vec<u8>)]) -> anyhow::Result<()> {
    let (_dir, path) = temp_db()?;
    let mut pager = new_pager(&path)?;
    let tree = BTree::create(&mut pager)?;
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    for (selector, value) in ops {
        let key = keys
            .get(selector.checked_rem(keys.len()).context("no keys")?)
            .context("key")?;
        let replaced = tree.put(&mut pager, key, value)?;
        let expected = model.insert(key.clone(), value.clone()).is_some();
        ensure!(
            replaced == expected,
            "put {key:?}: tree {replaced}, model {expected}"
        );

        for (k, v) in &model {
            ensure_value(get(tree, &pager, k)?.as_deref(), v, "model key")?;
        }
    }

    let entries = leaf_entries(&pager, tree.root())?;
    let expected: Vec<(Vec<u8>, Vec<u8>)> = model.into_iter().collect();
    ensure!(
        entries == expected,
        "leaf {entries:?} differs from model {expected:?}"
    );
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_put_matches_btreemap_model(
        keys in prop::collection::vec(
            prop::collection::vec(prop::sample::select(vec![b'a', b'b', b'c']), 0..12),
            1..=8,
        ),
        ops in prop::collection::vec(
            (any::<usize>(), prop::collection::vec(any::<u8>(), 0..8)),
            1..60,
        ),
    ) {
        run_model(&keys, &ops).map_err(|e| TestCaseError::fail(format!("{e:#}")))?;
    }
}
