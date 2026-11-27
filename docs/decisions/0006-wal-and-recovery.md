# 0006. Write-ahead log and crash recovery

- Status: proposed
- Date: 2026-11-27

## Context

Phase 3 makes the storage engine survive crashes. Until now every pager write
goes straight to the database file, and nothing is atomic across pages:

- ADR 0003 (Consequences and amendment d): allocate and free are two writes. A
  crash between them can leak a page or leave the file longer than
  `page_count` says, and the next `open` fails with `FileSizeMismatch`.
- ADR 0005 (invariant 7 and amendment 3, "Write order and crash windows"): a
  crash during a split, a merge, a rotation or an overflow write can leak
  pages or leave keys unreachable.

Goal of this phase: every `BTree::create`, `put` and `delete`, including the
splits, merges, root collapses, overflow chains, free-list changes, file growth
and file-header changes it causes, is **atomic and durable**. After a crash,
reopening the file shows each operation fully done or not done at all.

Constraints:

- No buffer pool yet (Phase 4). Page buffers are owned `Vec<u8>`.
- No multi-operation transactions or concurrency control yet (Phase 9, MVCC
  vs. locking still open). The design must not block them.
- No `unsafe`, no new dependency. The CRC32 (`pager::crc32`) and the
  little-endian codec (`pager::codec`) are reused.
- The common page header already reserves a `page_lsn: u64` at bytes 8..16
  (ADR 0003, option 3), and `page::set_page_lsn` and `page::page_lsn` exist.

Reading (_Database Internals_): Ch. 5 Recovery (Write-Ahead Log, Log
Semantics, Operation Versus Data Log, Steal and Force Policies, ARIES,
Checkpoints); Ch. 3 Checksumming.

## Options considered

### 1. Where the log lives

- **Separate file next to the database** (`<db>-wal`, the SQLite name). The
  path is the database path with `-wal` appended (`OsString::push`). Simple
  appends at the end, simple truncation.
- **A log region inside the database file.** One file, but the region needs
  its own space management and growth rules, and it competes with pages.
- **Rollback journal or shadow paging** (SQLite's default mode). The journal
  holds *before* images, and every commit must fsync the journal and then the
  database (force). It teaches nothing about LSNs and redo, which Phase 4 (the
  WAL rule for flushes) and Phase 9 (transactions) need.

**Chosen:** separate WAL file.

### 2. What a log record holds

- **Physical: full page images** (like PostgreSQL `full_page_writes`). Redo
  is "copy the image to the page", which is idempotent and needs no LSN
  comparison. It also repairs a page torn in the main file. The B+Tree code
  needs no redo function per operation. Cost: about one page of log per page
  touched.
- **Physiological** ("insert cell X in slot S of page P", ARIES style). Small
  records, but every page change needs a redo function, redo needs the
  "skip if `page_lsn >= lsn`" check, and torn pages need another fix.
- **Logical** ("put k v"). Smallest records, but redo needs a consistent tree
  to run against, so it brings the torn-page problem back.

**Chosen:** full page images. A physiological record kind can be added later
(see the `kind` field).

### 3. Undo, steal and force (Ch. 5, "Steal and Force Policies")

- **Steal** lets pages of an unfinished operation reach the database file.
  Then recovery needs undo (before images or CLRs) and an analysis pass
  (ARIES).
- **No-steal** keeps those pages in memory until the operation commits. Then
  recovery is redo only.
- **Force** writes and fsyncs the changed pages at every commit. **No-force**
  only makes the log durable at commit and lets the pages follow later.

**Chosen:** no-steal and no-force, so redo only. Without a buffer pool, the
pages of one operation already live in memory, and one operation touches a
bounded number of pages.

### 4. How an operation marks its boundaries

- **Explicit `begin` / `commit` / `abort` calls.** Easy to forget an `abort`
  on an error path.
- **A closure:** `pager.atomic(|p| ...)`. Commit on `Ok`, abort on `Err`, and
  no path can leave an operation open.

**Chosen:** the closure. A nested call joins the outer operation, so a later
multi-operation transaction can be an outer `atomic`.

### 5. Commit marker

- **Separate begin and commit records.** Needed when one operation's changes
  span many records (steal, or streaming logs).
- **One record per operation.** The record's CRC makes it atomic: a torn
  record fails the check, and a complete one is committed.

**Chosen:** one record per operation.

## Decision

### WAL file

The WAL file is `<db path>-wal`. All integers are little-endian.

Header (32 bytes, at offset 0):

| Offset | Size | Field              | Notes                                            |
|--------|------|--------------------|--------------------------------------------------|
| 0      | 8    | magic              | `STENGWAL`                                       |
| 8      | 2    | version: u16       | 1                                                |
| 10     | 2    | reserved           | 0                                                |
| 12     | 4    | page_size: u32     | must equal the database page size                |
| 16     | 8    | base_lsn: u64      | LSN of byte 0 of this WAL                        |
| 24     | 4    | reserved           | 0                                                |
| 28     | 4    | crc32: u32         | CRC32 of bytes [0, 28)                           |

Record (one per operation, appended at `wal_end`):

| Offset | Size                  | Field             | Notes                                      |
|--------|-----------------------|-------------------|--------------------------------------------|
| 0      | 4                     | crc32: u32        | CRC32 of bytes [4, total_len)              |
| 4      | 4                     | total_len: u32    | `24 + n * (4 + page_size)`                 |
| 8      | 8                     | lsn: u64          | `base_lsn + offset of this record`         |
| 16     | 1                     | kind: u8          | 1 = PageImages                             |
| 17     | 3                     | reserved          | 0                                          |
| 20     | 4                     | n: u32            | number of page images, >= 1                |
| 24     | n * (4 + page_size)   | images            | `(page_id: u32, image: [u8; page_size])`, ascending page id |

The file-header page (page 0) is logged as an image like any other page.

### LSN

`lsn = base_lsn + byte offset in the WAL`: a position in one endless log.
Every LSN is at least 32 (the header size), so it is never 0. A page LSN of 0
means "written before the WAL existed" (files from Phase 2). A checkpoint
raises `base_lsn` (below), so LSNs keep increasing across checkpoints.

### Page LSN (amends ADR 0003, option 3)

The page LSN is no longer reserved. `write_page` stamps
`page_lsn = lsn of the current operation`, then the checksum, into the
caller's buffer. In Phase 3 the page LSN is used only to check images on
replay (every image in a record must have `page_lsn == record lsn`) and for
inspection. Redo does not compare it, because it copies full images. Phase 4
will use it for the WAL rule (`page_lsn <= flushed_lsn` before a page is
flushed), and a physiological record kind would use "skip if
`page_lsn >= lsn`".

### Operations: `Pager::atomic`

```rust
pub fn atomic<T, E: From<PagerError>>(
    &mut self,
    f: impl FnOnce(&mut Self) -> Result<T, E>,
) -> Result<T, E>
```

- Not inside an operation: begin, run `f`, commit on `Ok`, abort on `Err`.
- Already inside one: run `f` as part of the outer operation.
- State: `op: Option<OpState>` with
  `OpState { dirty: BTreeMap<PageId, Vec<u8>>, header_before: FileHeader, lsn: u64 }`.
- Inside an operation:
  - `write_page` and the file-header write copy the stamped page into `dirty`.
  - `read_page` reads `dirty` first (read-your-writes). `allocate_page` needs
    this when it pops a page freed earlier in the same operation.
  - Growing the file only raises `page_count` in memory. The file length
    changes at apply time.
  - Reading a page that was allocated by growth but never written still fails
    the checksum, as today (ADR 0003, amendment a).
- Abort drops `dirty` and restores `header_before`. Nothing reached either
  file.
- **Autocommit:** each public writer (`write_page`, `allocate_page`,
  `free_page`, `set_catalog_root`) called outside an operation runs as its own
  operation.
- The B+Tree wraps the bodies of `BTree::create`, `put` and `delete` in
  `pager.atomic`. The split, merge and rebalance code does not change. A failed
  `put` or `delete` now changes nothing (ADR 0005, amendment 5).

**Owned data (AGENTS.md 5.1).** The dirty map owns a copy of every changed
page. The alternatives were tried in order:

1. Restructure or borrow: the B+Tree reuses or drops its buffers before the
   operation ends, so the pager cannot borrow them until commit.
2. Borrowed views: same problem, the owner of the bytes is gone.
3. Indices into a buffer: there is no buffer that outlives the operation and
   that the pager can index into.
4. `Cow`: every changed page needs the owned form at commit.
5. Arena: the buffer pool (Phase 4) is that arena. Its frames replace these
   copies then.
6. Writing through to the file instead of copying breaks no-steal.

So the copies stay, one `Vec<u8>` per changed page per operation, until
Phase 4.

### Commit protocol

1. Encode the record into a `Vec<u8>` owned by the pager and reused across
   operations.
2. `pwrite` it at `wal_end`.
3. `sync_data()` the WAL (unless `SyncMode::Off`).
4. `pwrite` every image to the database file, then
   `set_len(page_count * page_size)`.
5. Advance `wal_end`.
6. If `wal_end` passed the checkpoint threshold, checkpoint.

Errors:

- An error before step 2 (encoding): the operation is dropped, like an
  abort, and the pager stays usable.
- An error in step 2 (the WAL write): `Wal::append` writes and fsyncs in
  one call, so a write error cannot be told apart from an fsync error. It
  poisons the pager too. This is stricter than needed (for example, ENOSPC
  on the write poisons) but safe. The partial bytes past `wal_end` are
  overwritten by the next record, and recovery rejects them anyway (CRC,
  length or LSN).
- An error in step 3 or later **poisons** the pager. Every later call,
  reads included, returns `PagerError::Poisoned`. Reopening the file runs
  recovery. A failed fsync cannot be retried safely (the kernel may have
  dropped the dirty pages), and without the poison a later checkpoint could
  reset a WAL whose pages never reached the database file.

`SyncMode::{Full, Off}`, set with `Pager::set_sync_mode`, default `Full`.
`Off` skips every fsync but keeps the same write order. It exists for the test
suite only, so thousands of model-test operations do not each pay an fsync.

### Checkpoint (Ch. 5, "Checkpoints")

`Pager::checkpoint()`:

1. `fsync` the database file.
2. `pwrite` the WAL header with `base_lsn = base_lsn + wal_end`.
3. `fsync` the WAL.
4. `set_len(32)` on the WAL, and `wal_end = 32`.

Crash safety at each point:

- Before step 2 is durable: the old header and records are still there.
  Recovery replays them again, which is idempotent.
- After step 2 is durable, before step 4: the old records are still in the
  file but every one fails the LSN check, because a stale record at offset
  `o` has `lsn = old_base + o < new_base + o`.

When it runs:

- After a commit, when `wal_end` passed the threshold (default 4 MiB, set with
  `Pager::set_checkpoint_threshold`; `u64::MAX` means never, which crash tests
  use).
- At the end of recovery.
- In `Drop`, best effort, skipped when the pager is poisoned. A failed
  checkpoint leaves the WAL intact, so it is safe to ignore the error there.
- `Pager::sync()` delegates to `checkpoint()`, so its call sites stay. It now
  takes `&mut self`.

### Recovery (on `Pager::open`, before the current open checks)

1. Open the WAL. If it does not exist (a Phase 2 file, or a crash between
   creating the database and creating the WAL), create it with `base_lsn = 0`.
2. Decode the WAL header. A bad header is `WalError::CorruptHeader`: refuse to
   open, do not guess. A page size that differs from a readable database
   header is `WalError::PageSizeMismatch`.
3. Read the WAL into memory (shortcut: the size is bounded by the checkpoint
   threshold).
4. Scan records from offset 32. A record is valid if, in this order:
   1. `total_len >= 24` and `total_len == 24 + n * (4 + page_size)`;
   2. it fits in the file;
   3. `lsn == base_lsn + offset`;
   4. the CRC matches;
   5. `kind` is known;
   6. every image has a valid page checksum and `page_lsn == lsn`.

   The first invalid record is the end of the log.
5. `pwrite` every image of every valid record to the database file.
6. Read page 0 and `set_len(page_count * page_size)`.
7. `fsync` the database, then checkpoint (reset the WAL).

Then the existing open checks (ADR 0003, amendment b) run unchanged.
`Pager::recovery_report()` returns `RecoveryReport { records, pages }`.

Compared with ARIES: there is no analysis pass, because every complete record
is a committed operation; no undo pass, because of no-steal; and redo is one
forward scan with no LSN comparison, because images are idempotent.

### Torn writes

- **A database page torn while it is applied:** every page written since the
  last checkpoint has a full image in the WAL, so replay overwrites it. This
  includes page 0.
- **A torn WAL tail:** the length, LSN or CRC check fails, and that operation
  was never acknowledged to the caller.
- **The WAL header:** 32 bytes fit in one disk sector, so its write is assumed
  atomic, as PostgreSQL assumes for `pg_control`. A header that fails its CRC
  is reported, never ignored.

### Creating a database

`Pager::create` writes and fsyncs page 0, creates the WAL (header and fsync),
then fsyncs the parent directory so both new directory entries survive a
crash. The database file format does not change: `format_version` stays 2.

The allocation gap of ADR 0003, amendment d is closed: a page added by growth
reaches the file only at apply time, after the WAL fsync, and `set_len` runs
after the images are written.

## Consequences

- Every B+Tree operation is atomic and durable. The crash windows of ADR 0003
  and ADR 0005 are closed.
- One fsync per operation in `SyncMode::Full` (about 1–3 ms on a disk). Group
  commit is left for later.
- Write amplification: each changed page is written twice (WAL and database),
  and each operation's pages are held twice in memory (dirty map and record
  buffer). A put with a large value logs its whole overflow chain.
- `total_len` is a u32, so a record over 4 GiB is rejected with
  `WalError::RecordTooLarge`. Document size limits come later.
- **A missing WAL is recreated empty** with `base_lsn = 0`. Operations that
  were committed but not yet checkpointed are lost without an error, and LSNs
  can go backwards. Accepted for Phase 3. A file header v3 with a
  `checkpoint_lsn` and a "WAL required" check would detect it.
- **Corruption in the middle of the log** looks like a torn tail: the records
  after it are dropped without an error, as in PostgreSQL.
- A poisoned pager refuses reads too. The only way out is to reopen.
- No format-version bump: a Phase 2 binary could open a database that still
  has pending WAL records. No old binaries are deployed.
- I/O in `Drop` (the best-effort checkpoint). An explicit `close` would be
  cleaner but changes many tests.
- Phase 9 can build transactions on an outer `atomic` while they fit in memory
  (no-steal). If it chooses steal or MVCC, it adds undo records and an
  analysis pass; the `kind` byte and the WAL `version` leave room for them.
- Amends ADR 0003 (page LSN used; amendment d closed) and ADR 0005
  (amendment 5). No new dependency, no `unsafe`.
