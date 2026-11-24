# Phase 2: B+Tree

Goal: an ordered, persistent B+Tree over the pager that serves as the building
block of the document database. One database file holds many trees: one
primary tree per model (`_id` → document) and one tree per secondary index
(encoded field value + `_id` → empty value). Each tree is identified by its
root page id.

Out of scope: WAL and crash recovery (Phase 3; a crash during a split can
corrupt a tree until then), buffer pool (Phase 4), the catalog itself
(Phase 6; this phase only reserves its root in the file header), concurrency
control, order-preserving key encoding for document types (Phases 5 and 8),
right-only append optimization, prefix and suffix compression.

Reading (_Database Internals_): Ch. 2 B-Tree Basics (lookups, node splits,
node merges); Ch. 4 Implementing B-Trees (page header, sibling links,
rightmost pointers, overflow pages, binary search, propagating splits and
merges, breadcrumbs, rebalancing, vacuum).

## Design constraints

- Keys are unique byte strings compared with `memcmp` order. Non-unique
  indexes append the `_id` to the key to make it unique.
- Keys have a maximum size. Values may be larger than a page and spill to
  overflow pages.
- The root page id of a tree never changes, so the catalog can reference it.
  A root split moves the old root's contents to a new page.
- Leaves are linked to their right neighbor only. Forward scans follow the
  link; reverse scans use the cursor's path from the root (ADR 0005).
- Sequential inserts use right-only appends, so leaves fill completely.
- Range scans support inclusive, exclusive and unbounded ends.
- Split propagation uses a stack of breadcrumbs (the page ids on the path from
  the root), not recursion.
- The `StorageEngine` trait works on trees: create a tree, then get, put,
  delete and scan by tree id.

## How each step works

The agent implements each step following the approved plan, with tests, and
explains the change for review. The owner reviews the diff before the commit.
See [AGENTS.md](../../AGENTS.md) sections 3 and 4.

## Steps

| #   | Step                          | Goal                                                                                   | Reading (_Database Internals_)                                 | Status  |
| --- | ----------------------------- | -------------------------------------------------------------------------------------- | -------------------------------------------------------------- | ------- |
| 0   | Node format ADR               | ADR 0005: leaf and internal nodes, overflow pages, right links, file header v2        | Ch. 4 Page Header, Sibling Links, Rightmost Pointers, Overflow Pages | Done    |
| 1   | File header v2                | Add `catalog_root`, bump the format version                                            | Ch. 3 Versioning                                               | Done    |
| 2   | Node views                    | Read and write views for leaf and internal nodes; search a key inside a node          | Ch. 4 Binary Search                                            | Done    |
| 3   | Engine trait and tree handle  | Draft `StorageEngine` trait (implemented after step 10); `BTree::create` and `BTree::open` by root page id | —                                                  | Done    |
| 4   | Lookup                        | `get`: descend from the root to a leaf                                                 | Ch. 2 B-Tree Lookup Algorithm                                  | Done    |
| 5   | Insert without splits         | `put` into a root leaf, with upsert                                                    | Ch. 2 Node Splits                                              | Done    |
| 6   | Leaf split                    | Split a full leaf, with right-only appends; keep the root page id stable              | Ch. 2 Node Splits; Ch. 4 Propagating Splits and Merges         | Done    |
| 7   | Internal split                | Split internal nodes and propagate through breadcrumbs, any height                    | Ch. 4 Breadcrumbs                                              | Done    |
| 8   | Overflow pages                | Store values larger than the inline limit in a chain of overflow pages                | Ch. 4 Overflow Pages                                           | Done    |
| 9   | Range cursor                  | Forward and reverse scans with inclusive, exclusive and unbounded ends                | Ch. 4 Sibling Links, Breadcrumbs                               | Done    |
| 10  | Delete                        | Remove a key and free its overflow pages, without rebalancing                          | —                                                              | Planned |
| 11  | Rebalancing                   | Merge and redistribute underfull nodes, collapse the root, free empty pages           | Ch. 2 Node Merges; Ch. 4 Rebalancing                           | Planned |
| 12  | Verification and model tests  | `verify_tree` invariants; property tests against a `BTreeMap` model, many trees per file | —                                                            | Planned |
| 13  | REPL on the B+Tree            | The test REPL stores its data in a B+Tree file                                         | —                                                              | Planned |
| 14  | Docs                          | Update README roadmap, ADRs and this file                                              | —                                                              | Planned |

One commit per step on `feat/btree`.
