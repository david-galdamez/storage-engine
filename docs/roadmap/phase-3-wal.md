# Phase 3: WAL and crash recovery

Goal: every B+Tree operation (`create`, `put`, `delete`), with all the pages,
free-list changes, file growth and file-header changes it causes, is atomic
and durable. After a crash, reopening the database shows each operation fully
done or not done at all.

Related decisions: [ADR 0006](../decisions/0006-wal-and-recovery.md) (WAL and
recovery), which amends [ADR 0003](../decisions/0003-pager-file-format.md)
(page LSN, file growth) and [ADR 0005](../decisions/0005-btree-node-format.md)
(amendment 5: atomic operations).

Out of scope: buffer pool and the steal policy with its WAL rule on flush
(Phase 4), multi-operation transactions, undo, isolation and concurrency
(Phase 9), group commit, logical or physiological records, fuzzy checkpoints,
checking that a WAL belongs to its database beyond the page size, WAL
archiving and point-in-time recovery, a double-free guard in `free_page`.

Reading (_Database Internals_): Ch. 5 Recovery (Write-Ahead Log, Log
Semantics, Operation Versus Data Log, Steal and Force Policies, ARIES,
Checkpoints); Ch. 3 Checksumming.

## Design constraints

- The WAL is a separate file, `<db>-wal`, with full page images: redo only,
  no-steal, no-force.
- One operation is one WAL record. The record's CRC makes it atomic; there is
  no separate commit record.
- `lsn = base_lsn + byte offset in the WAL`. LSNs only increase, across
  checkpoints too. `write_page` stamps the page LSN.
- Operations are closures: `pager.atomic(|p| ...)` commits on `Ok`, aborts on
  `Err`, and nests by joining the outer operation. Writes outside an operation
  autocommit.
- Inside an operation, reads see the operation's own writes.
- Nothing of an operation reaches the database file before its record is
  durable in the WAL.
- An error at or after the WAL fsync poisons the pager until it is reopened.
- `SyncMode::Off` is for tests only.
- The database file format stays at version 2. No new dependency, no `unsafe`.

## How each step works

The agent implements each step following the approved plan, with tests, and
explains the change for review. The owner reviews the diff before the commit.
See [AGENTS.md](../../AGENTS.md) sections 3 and 4.

## Steps

| #   | Step                 | Goal                                                                                         | Reading (_Database Internals_)                     | Status |
| --- | -------------------- | -------------------------------------------------------------------------------------------- | -------------------------------------------------- | ------ |
| 0   | ADR and roadmap      | ADR 0006, ADR 0005 amendment 5 (proposed), ADR 0003 note, this file                          | Ch. 5 Recovery; Ch. 3 Checksumming                 | Done   |
| 1   | WAL codec            | `wal/format.rs`: header and record encode/decode; `LogReader` over a borrowed `&[u8]`        | Ch. 5 Log Semantics; Ch. 3 Checksumming            | Done   |
| 2   | WAL file             | `wal/log.rs`: `Wal` create, open, append, reset; `SyncMode`; `wal_path`                      | Ch. 5 Write-Ahead Log                              | Done   |
| 3   | Recovery on open     | `wal/recovery.rs`: replay valid records on `Pager::open`; `create` makes the WAL             | Ch. 5 ARIES (redo only)                            | Done   |
| 4   | Logged writes        | `Pager::atomic`, autocommit, read-your-writes, deferred growth, page LSN, commit, `Poisoned` | Ch. 5 Steal and Force Policies                     | Done   |
| 5   | Checkpoint           | `checkpoint()`, `sync()` delegates, threshold, `set_sync_mode`, best-effort `Drop`           | Ch. 5 Checkpoints                                  | Done   |
| 6   | Atomic B+Tree ops    | Wrap `BTree::create`, `put`, `delete` in `pager.atomic`; ADR 0005 amendment 5 accepted       | Ch. 5; Ch. 4 Propagating Splits and Merges         | To do  |
| 7   | Crash tests          | WAL cut at every byte, partial and torn page applies, `verify_file` clean after each case    | Ch. 5 Recovery                                     | To do  |
| 8   | Crash property test  | Random put/delete and a random crash point against a `BTreeMap` model                        | —                                                  | To do  |
| 9   | REPL                 | "recovered N operations" message on open, `checkpoint` command, demo removes `-wal`          | —                                                  | To do  |
| 10  | Docs                 | README roadmap, AGENTS.md section 9, inspect guide WAL section, ADR 0006 accepted             | —                                                  | To do  |

One commit per step on `feat/wal`.

## Known limits and follow-ups

- One fsync per operation in `SyncMode::Full`. Group commit comes later.
- A missing WAL is recreated empty: committed operations that were not
  checkpointed are lost without an error (ADR 0006, Consequences). A file
  header v3 with `checkpoint_lsn` would detect it.
- Corruption in the middle of the WAL drops every record after it without an
  error.
- Full page images double the bytes written per changed page, and an
  operation's pages are held in memory twice until commit.
- Records are limited to 4 GiB (`total_len` is a u32).
- Recovery reads the whole WAL into memory, bounded by the checkpoint
  threshold.
- The WAL header write is assumed atomic (one sector).
- Dirty-page copies are owned `Vec<u8>` until the buffer pool (Phase 4).
