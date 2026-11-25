# 0005. B+Tree node format, overflow pages and value access

- Status: accepted
- Date: 2026-11-19

## Context

Phase 2 builds a B+Tree on top of the pager (ADR 0003). The tree is not the
final product: it is the building block of a document database. That sets the
requirements:

- **Many trees per file.** Each model has a primary tree (`_id` → document)
  and each secondary index has its own tree (encoded field value + `_id` →
  empty value). The catalog (Phase 6) will map names to trees, so a tree needs
  an identifier that never changes.
- **Large values.** A document can be larger than a page.
- **Small values.** Index entries have empty values, so most of an index cell
  is key.
- **Ordered scans in both directions.** Range filters (`$gt`, `$lt`) and
  `$sort` read leaves in order, ascending or descending.
- **Unique keys compared as bytes.** Non-unique indexes append the `_id` to the
  key. Encoding document values so that byte order matches value order is the
  job of Phases 5 and 8. The tree only compares bytes (`memcmp` order).
- **Read without copying when possible.** Phase 5 wants in-place field access,
  so a small value should be readable as a slice of the page.

ADR 0003 left room for this: byte 24 of a slotted page (`flags`, "leaf or
internal"), bytes 25..32 ("rightmost child or sibling link"), and bytes 44.. of
the file header ("root or catalog page id, with a version bump").

There is no buffer pool until Phase 4, and no WAL until Phase 3.

## Options considered

### 1. How a page says it is a leaf or an internal node

- **A flag byte on a slotted page (`page_type = 3`)**: uses byte 24 as ADR 0003
  planned. It leaves 7 bytes at 25..32: room for one 4-byte link, but at an
  odd offset, and every node view has to check the flag by hand.
- **New page types**: 4 = B+Tree leaf, 5 = B+Tree internal. The page type
  check already runs on every view, so a node view rejects a page of the wrong
  kind for free. Byte 24 becomes free, which leaves 8 bytes at 24..32.

**Chosen:** new page types. Both node types keep the slotted layout of ADR 0003
(cell count, fragmented bytes, `free_end`, slot directory at 32), so insert,
remove and compaction are reused as they are. Page type 3 stays as a plain
slotted page.

### 2. Links for reverse scans

- **Previous and next links on leaves**: a cursor moves to the neighbor leaf
  with one page read in either direction. But:
  - A leaf split or merge writes one more page: the right neighbor, whose
    `prev` must point to the new leaf.
  - Two links must stay consistent, so a crash before the WAL exists has more
    ways to break the tree.
  - With concurrency (Phase 9), a forward scan and a backward scan can
    deadlock: each holds the latch of its leaf and waits for the other's.
    Avoiding it needs extra latching rules, such as try-latch and retry when
    moving left.
- **Next link only, with the cursor's stack of parents for reverse scans**: a
  forward scan follows `next_leaf`. A reverse scan uses the path the cursor
  took from the root: it climbs to the nearest parent where it can step one
  child to the left, and descends to the rightmost leaf of that child. Most
  steps stay under the same parent, whose page the cursor already holds. Right
  links only are also what B-link trees (Lehman and Yao, *Database Internals*
  ch. 5) use for concurrency.

**Chosen:** next link only. Reverse scans use the cursor's path. The split
writes fewer pages, and the Phase 9 latching has one direction to deal with.

### 3. Where the separator keys live in an internal node

An internal node has N keys and N + 1 children.

- **Every cell holds a child and a key, and the last child goes in the page
  header** ("rightmost pointer", *Database Internals* ch. 4). Every cell has
  the same shape.
- **One cell holds only a child**: the shapes differ, and the cell with only a
  child must stay first or last in the slot order.

**Chosen:** rightmost pointer in the header, at bytes 24..28.

### 4. Root page id

- **The root page id changes when the root splits**: the catalog must be
  updated on every root split and every root collapse.
- **The root page id never changes**: on a root split, the root's cells move
  to two new pages and the root becomes an internal node over them. On a root
  collapse, the only child is copied into the root and freed.

**Chosen:** the root page id never changes. It is the tree id.

### 5. Values that do not fit in a page

- **Size limit on values**: simple, but a document database needs documents
  larger than a page.
- **Overflow chain, the whole value out of line**: the leaf cell holds the
  value length and the first overflow page. A value is either all inline or
  all in the chain.
- **Overflow chain with an inline prefix** (SQLite): the first bytes of the
  value stay in the leaf, so a field near the start of a document can be read
  without reading the chain. More rules to decide how big the prefix is.

**Chosen:** the whole value out of line. An inline prefix can come later with
a format version bump, once the document format (Phase 5) shows whether it
helps.

### 6. Size limits

A split must always produce two nodes that fit, and an internal node must keep
a useful fan-out. Both hold if every entry (cell plus its 4-byte slot) uses at
most a quarter of the usable space (`page_size - 32`):

- `max_cell = (page_size - 32) / 4 - 4`
- `max_key = max_cell - 11`. The largest leaf cell for a given key is the
  overflow form (11 bytes of overhead, see the layout below), so every key up
  to `max_key` fits in a leaf and in an internal cell (4 bytes of overhead).
- A value is inline if `3 + key_len + value_len <= max_cell`. Otherwise it goes
  to an overflow chain.

| page size | max_cell | max_key |
|-----------|----------|---------|
| 512       | 116      | 105     |
| 4096      | 1012     | 1001    |
| 65536     | 16372    | 16361   |

Other options: a fixed key limit (for example 1024 bytes) would not fit small
pages; a limit of half a page gives a fan-out of 2, which makes deep trees.

### 7. How a lookup returns a value

There is no buffer pool, so the bytes of a page live in a buffer that the
caller of `Pager::read_page` owns. AGENTS.md section 5.1 asks to try borrowing
first:

1. **Restructure / borrow from the page buffer.** `get` reads the leaf into a
   buffer that only lives during the call, so it cannot return a slice of it.
   A cursor, however, owns its current leaf buffer and can lend slices of it
   until the next move. This works for inline values.
2. **Borrowed view over the value.** An overflow value is spread over several
   pages, so it is never one slice. A `ValueRef<'a>` can describe it instead:
   `Inline(&'a [u8])` or `Overflow { len, first_page }`. The caller reads the
   chain only if it needs the value. A query that only needs keys (a count, a
   covered index) never reads it.
3. **Indices into a buffer**: same limit as 1 for `get`.
4. **`Cow`**: does not help. Both forms already have no copy until the caller
   asks for the bytes.
5. **Arena**: needs an owner that lives longer than the call. The buffer pool
   (Phase 4) is that owner.
6. **Owned data**: the caller passes a `&mut Vec<u8>` that receives the value.
   The caller can reuse it across calls, so a loop of lookups allocates once.

**Chosen:**

- The cursor lends `(&[u8], ValueRef<'_>)`, both borrowed from the cursor's
  leaf buffer and valid until the next move. The cursor is a lending cursor
  (`next(&mut self)`), not a `std::iter::Iterator`.
- `ValueRef::Overflow` is read with a call that appends the value to a
  caller-owned `&mut Vec<u8>`.
- `get` copies the value into a caller-owned `&mut Vec<u8>` and returns
  whether the key exists. Zero-copy lookups use a cursor positioned on the key.
- Page buffers are `Vec<u8>` of `page_size` bytes, owned by the operation or
  by the cursor. The page size is only known at run time, and a split needs
  several buffers at once, so a fixed `[u8; 65536]` on the stack is not used:
  up to 256 KiB of stack per operation is too much for async tasks later.

The buffer pool (Phase 4) will replace these owned buffers with borrowed page
frames, and this section will be revisited then.

## Decision

All integers are little-endian, as in ADR 0003. `PageId` 0 means "none".

### Page types

| Value | Page type        |
|-------|------------------|
| 4     | B+Tree leaf      |
| 5     | B+Tree internal  |
| 6     | Overflow         |

### Node header (leaf and internal)

Bytes 0..24 and the slot directory from 32 are the same as the slotted page of
ADR 0003, and so are its invariants 1–8. Bytes 24..32 change meaning:

| Offset | Size | Leaf                     | Internal                        |
|--------|------|--------------------------|---------------------------------|
| 24     | 4    | `next_leaf: u32`, 0 = none | `rightmost_child: u32`, never 0 |
| 28     | 4    | reserved, 0              | reserved, 0                     |

Slotted operations (insert, remove, compact) never write bytes 24..32.

### Leaf cell

| Offset            | Size      | Field                                  |
|-------------------|-----------|----------------------------------------|
| 0                 | 2         | `key_len: u16`                         |
| 2                 | 1         | `flags: u8`: bit 0 = overflow, other bits 0 |
| 3                 | `key_len` | key                                    |
| 3 + key_len       | rest      | inline: the value (may be empty)        |
| 3 + key_len       | 4         | overflow: `value_len: u32`             |
| 7 + key_len       | 4         | overflow: `first_overflow: u32`, never 0 |

The length of an inline value is the cell length minus `3 + key_len`. An
overflow cell is exactly `11 + key_len` bytes. A value is inline exactly when
the rule in option 6 says so, so the same key and value always give the same
cell.

### Internal cell

| Offset | Size | Field          |
|--------|------|----------------|
| 0      | 4    | `child: u32`, never 0 |
| 4      | rest | separator key  |

### Overflow page (page type 6)

| Offset | Size | Field                                  |
|--------|------|----------------------------------------|
| 16     | 4    | `next_overflow: u32`, 0 = end of chain |
| 20     | 4    | `data_len: u32`                        |
| 24     | `data_len` | value bytes                      |
| 24 + data_len | rest | zero                           |

Every page of a chain except the last is full (`data_len = page_size - 24`).
The sum of `data_len` over the chain equals the `value_len` in the leaf cell.

### File header, format version 2

| Offset | Size | Field                                     |
|--------|------|-------------------------------------------|
| 44     | 4    | `catalog_root: u32`, 0 = none, otherwise < `page_count` |
| 48     | rest | reserved, 0                               |

`format_version` becomes 2. Version 1 files are rejected with
`UnsupportedVersion`. The only version 1 files are test and demo files, so
there is no migration.

### Tree invariants

1. A tree is identified by its root page id, which never changes. A new tree
   is one empty leaf.
2. All leaves are at the same depth.
3. Keys are unique, and the cells of every node are in strictly ascending
   byte order.
4. In an internal node with keys `K0 .. K(n-1)`, children `C0 .. C(n-1)` and
   rightmost child `Cn`, every key in the subtree of `Ci` is `>= K(i-1)` and
   `< Ki`. The bounds that do not exist are the bounds of the node itself. A
   lookup of `k` goes to the first `Ci` with `k < Ki`, or to `Cn`.
5. A leaf split moves the upper half by bytes to a new right leaf and copies
   the first key of the new leaf up as the separator. An internal split moves
   the middle key up (it is not kept in either half): the left node's
   rightmost child becomes that cell's child. The middle is chosen by bytes,
   like the leaf split: cell `m` of the `n` cells (the old cells plus the new
   one), with `m` in `1..=n-2` so both halves keep at least one cell.

   Exception, right-only appends (*Database Internals* ch. 4): when the new
   key goes after the last cell of the rightmost leaf of the tree, the full
   leaf keeps all its cells and the new key starts a new right leaf. With
   sequential keys (for example a time-ordered `_id`), a split by halves
   would leave every left leaf half empty forever, because no later key falls
   into it. When the rightmost internal node on the path is full, it keeps
   all its cells but the last one, whose key moves up (its child becomes the
   node's rightmost child), and the new right node holds only the new
   separator.
6. The leaves form one singly linked list in key order: `next_leaf` of a leaf
   is the leaf to its right, and the last leaf has `next_leaf = 0`. Following
   `next_leaf` from the leftmost leaf visits every leaf exactly once.
7. Every overflow page belongs to exactly one leaf cell. Removing or replacing
   the cell frees its whole chain.
   A chain is written before the leaf that points to it, and when a value is
   replaced the old chain is freed only after the leaf was written. A crash
   can leak pages but never leaves a cell pointing to a freed page (until the
   WAL, Phase 3). Delete follows the same order: it writes the leaf without
   the cell before it frees the chain.
8. Until rebalancing (Phase 2, step 11), deletes can leave empty leaves. They
   stay linked, and cursors skip them. Internal nodes always have at least
   one cell, except for the moment inside a root collapse.

The thresholds for merging and redistributing nodes are decided in step 11 and
added to this ADR as an amendment.

## Consequences

- One file holds any number of trees. The catalog tree (Phase 6) stores the
  other trees' root ids, and the file header points to the catalog.
- A leaf split writes three pages (the leaf, the new leaf and the parent),
  plus the allocator's writes. The new leaf takes the old `next_leaf`, so the
  right neighbor is not touched.
- A forward scan reads one page per leaf. A reverse scan reads one page per
  leaf while it stays under the same parent, and climbs and descends again
  when it crosses to another parent. The cursor keeps one page buffer per
  level of its path.
- Sequential inserts fill leaves completely (right-only appends). Random
  inserts leave nodes between half and fully used.
- A root split allocates two pages, because the root keeps its page id.
- Keys have a maximum size that depends on the page size (105 bytes at 512,
  1001 bytes at 4096). Higher layers must reject longer keys with a clear
  error. Index keys at 512-byte pages are tight, but 512 is a test size.
- Large values cost one page read per `page_size - 24` bytes, and a document
  of a few hundred bytes past the inline limit uses a whole overflow page.
- Lookups copy values into a caller buffer. Zero-copy reads go through a
  cursor. Page buffers are owned `Vec<u8>` until Phase 4.
- A crash during a split or an overflow write can leave a tree inconsistent
  until the WAL exists (Phase 3).
- Format version 2. Amends ADR 0003: byte 24 (`flags`) is no longer reserved
  on node pages, and bytes 44..48 of the file header hold `catalog_root`.

## Amendment 1 (2026-11-20): owned separator during splits

Phase 2, step 7 propagates a split up the tree. At each level a separator key
goes into the parent. That key is the first key of the new right leaf, or the
key promoted out of an internal node. `put` keeps it in one owned `Vec<u8>`.
AGENTS.md section 5.1 asks to try borrowing first:

1. **Restructure or borrow from the source buffer.** The source buffer (the
   right leaf, or the internal node that split) moves into the list of pending
   writes while the next level is processed. A borrow from it conflicts with
   pushing more buffers to that list. For the leaf, the right buffer is itself
   a pending write.
2. **Borrowed view into the buffer.** Same problem: the owner of the bytes
   moves.
3. **Indices or offsets into the buffer.** Same problem. The index would need
   a buffer that stays in place, and no buffer does.
4. **`Cow`.** It does not help. Every level of a split needs the owned form.
5. **Arena.** The buffer pool (Phase 4) is that owner. Revisit this decision
   then.
6. **Owned data.** One `Vec<u8>` is allocated once per `put` that splits and
   reused at every level (`clear` and `extend_from_slice`). It holds at most
   `max_key_len` bytes. A `put` that does not split allocates nothing for it.

**Decision:** option 6. The page buffers stay owned `Vec<u8>` as in option 7
of this ADR.

## Amendment 2 (2026-11-25): scan at the engine level

Phase 2, step 10c adds `StorageEngine::scan`. Option 7 of this ADR lends
`(&[u8], ValueRef)` from the B+Tree cursor. `ValueRef` is a B+Tree detail
(inline bytes or an overflow chain), so it does not appear in the engine
trait. At the engine level the choice is the same one `get` made: **lend the
key; copy the value into a caller-owned buffer only when asked.**
`EngineCursor::next_entry` takes `Option<&mut Vec<u8>>`. With `None`, no value
is read and no overflow page is touched. With `Some`, the buffer is cleared on
every call and holds the value of the returned key.

**Decision:** no new owned-data case. Keys stay borrowed from the cursor's
leaf buffer, and the value buffer is owned by the caller and reused across
entries. `BTreeScan` holds only borrows (`Cursor<'a>` and `&'a Pager`).

## Amendment 3 (2026-11-25): leaf rebalancing after delete

Phase 2, step 11a lets `delete` rebalance leaves. This amendment covers the
leaf level, the root collapse over two leaves, and the order of the writes.
Step 11b does the same for internal nodes and lifts the restrictions below.
It supersedes the "Until rebalancing" wording of invariant 8 only for leaves
that the rules below fix: a leaf that is left underfull is still valid.

**Underflow threshold.** Let `U = page_size - 32` be the usable bytes of a
node and `used = U - total_free` the bytes of its cells and slots. A non-root
node is *underfull* when `used * 4 < U`, so a node without cells is always
underfull. The minimum fill is `ceil(U / 4)`: 120, 1016 and 16376 bytes for
512, 4096 and 65536 (`min_node_fill`). The root is never underfull and a root
leaf may be empty. The threshold is a quarter, while a split leaves each half
at about half (`U / 2`). That gap is the hysteresis: after a split or a
rebalance, many deletes are needed before the node is underfull again, so a
workload that alternates `put` and `delete` at the same place does not merge
and split on every call. The maximum entry is also a quarter of `U` (option
6), so two underfull nodes always fit together in one page.

**Which sibling.** The sibling must have the same parent: the left one
(position `pos - 1`) if the leaf is not the first child, otherwise the right
one (`pos + 1`). The parent is the bottom breadcrumb of the descent. Only
`next_leaf` links exist (option 2), and a merge always goes *into the left
page* of the pair: the left page keeps its id and takes the `next_leaf` of the
right page, the right page is freed. The leaf that points to the left page is
not touched, so no `prev` link and no neighbour of another parent is needed.

**Merge before redistribute.** If `used(left) + used(right) <= U` the pair is
merged: the parent loses the separator between them (the pointer after it now
leads to the left page). Otherwise the cells of both leaves are cut again by
bytes with the rule of a leaf split (never the append case), and the
separator in the parent becomes the first key of the new right leaf. A merge
frees a page and shrinks the parent, which is why it is preferred. Moving a
cell moves only its reference to an overflow chain, never the chain
(invariant 7 does not change).

**Root collapse (height 2).** If a merge leaves the root, an internal node
over leaves, without cells, the merged leaf is written to the root page,
which keeps its id (option 4), and both leaf pages are freed. The merged leaf
has no next leaf, because it holds the only two leaves of the tree.

**Write and free order.** The pages are planned in memory and written
bottom-up: the pair pages, then the parent (the root last). The emptied pages
are freed after every write succeeded, and the value's overflow chain is
freed last. A crash can leak pages but nothing points to a freed page. A crash
between the writes of the pair and the write of the parent during a
redistribution can leave keys that moved across the separator unreachable
(the parent still routes them to the other leaf) until the WAL exists
(Phase 3).

**Separator.** The separator of a redistribution is copied into one owned
`Vec<u8>`, like the one of a split (amendment 1), for the same reason: the
buffer that holds the key moves into the pending writes. No new owned-data
case.

**Restrictions of step 11a.** In these cases the underfull leaf is not
rebalanced and stays as it is, which invariant 8 allows. Step 11b lifts them:

- a merge that would leave a non-root parent without cells;
- a redistribution whose new separator does not fit in the parent;
- any work above the parent of the leaf: after a merge the parent is not
  checked for underflow, so it can stay underfull (but it always keeps at
  least one cell), and there is no collapse at height 3 or more.
