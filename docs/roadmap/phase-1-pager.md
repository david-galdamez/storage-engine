# Phase 1: Pager

Goal: store fixed-size pages in one database file, reuse freed pages, and lay
out variable-size cells inside a page. This is the base for the B+Tree in
Phase 2.

Out of scope: page cache (Phase 4), WAL and crash recovery (Phase 3), B+Tree
(Phase 2), REPL integration.

Related decisions: [ADR 0001](../decisions/0001-page-size.md) (page size),
[ADR 0002](../decisions/0002-test-dev-dependencies.md) (test dependencies),
[ADR 0003](../decisions/0003-pager-file-format.md) (file format). For steps
4–8, design each layout yourself first, then compare it with ADR 0003 and
amend the ADR if your design is better.

## How each step works

1. Claude gives a brief: goal, constraints, reading, and questions the design
   must answer.
2. The owner designs and implements the API, types, and errors.
3. Claude reviews (questions before answers) and writes tests against the
   owner's API. Hints only on request.
4. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
   `cargo test` pass, then one commit per step.

## Steps

| #   | Step                       | Goal                                                                                 | Reading (_Database Internals_)                                                                               | Status |
| --- | -------------------------- | ------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------ | ------ |
| 0   | File format ADR            | Agree on the on-disk format before coding                                            | Ch. 3 Page Structure, Slotted Pages, Versioning, Checksumming; Ch. 4 Page Header                             | Done   |
| 1   | Lib target and test deps   | Library crate, `proptest`, `tempfile`, empty pager modules                           | —                                                                                                            | Done   |
| 2   | Byte codec and basic types | Read and write integers in a page buffer safely; types for page number and page size | Ch. 3 Binary Encoding                                                                                        | Done   |
| 3   | CRC32                      | Hand-written checksum to detect corrupted pages                                      | Ch. 3 Checksumming                                                                                           | Done   |
| 4   | Common page header         | What every page carries: type, checksum, room for the WAL                            | Ch. 3 Page Structure; Ch. 5 Recovery (why pages need an LSN)                                                 | Done   |
| 5   | File header                | Page 0: identify the file and describe the database                                  | Ch. 4 Magic Numbers; Ch. 3 Versioning                                                                        | Done   |
| 6   | Pager with page I/O        | Create and open a database file, read and write pages, grow the file                 | Ch. 3 General Principles; Ch. 4 Page Header                                                                  | Next   |
| 7   | Free list                  | Reuse freed pages instead of always growing the file                                 | Ch. 4 Vacuum and Maintenance; SQLite file format, "The Freelist"                                             |        |
| 8   | Slotted page               | Store, read, and remove variable-size cells in a page, in order                      | Ch. 3 Slotted Pages, Cell Layout, Managing Variable-Size Data; Ch. 4 Binary Search with Indirection Pointers |        |
| 9   | Compaction                 | Reclaim space left by deleted cells                                                  | Ch. 4 Fragmentation, Page Defragmentation                                                                    |        |
| 10  | Property tests             | Random operations checked against a simple model                                     | —                                                                                                            |        |
| 11  | Demo example               | Create a sample file to inspect with `xxd`                                           | —                                                                                                            |        |
| 12  | Docs                       | Update README roadmap and ADRs                                                       | —                                                                                                            |        |

Steps 2–9 are written by the owner. Steps 10 and 11 are supporting code
written by Claude. Step 12 is documentation.
