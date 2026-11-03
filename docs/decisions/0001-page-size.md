# 0001. Page size

- Status: accepted
- Date: 2026-11-03

## Context

The pager reads and writes fixed-size pages at offset `page_id * page_size`.
The page size must be chosen before any on-disk format exists, because every
later structure (file header, B+Tree nodes, document cells, overflow pages,
WAL frames) depends on it.

AGENTS.md section 10 lists the page size as an open decision: 4 KiB vs. 8 KiB.

The choice affects:

- B+Tree fanout, and therefore tree height and I/O per lookup.
- The size at which a document no longer fits inline and needs overflow pages.
- Write amplification: how many bytes are rewritten for a small change.
- Buffer pool granularity: how much cache is wasted on cold data.
- Torn-write risk once the WAL exists.
- Contention if page-level locking is chosen. The concurrency model (MVCC vs.
  locking) is still open, so this factor is not yet decided either way.

## Options considered

### Option A: 4 KiB

Pros:

- Matches the x86-64 OS memory page, the default ext4 block size, and the
  physical sector size of modern SSDs (Advanced Format). One database page maps
  to one filesystem block.
- Lower torn-page risk on power loss than a page spanning two 4 KiB blocks.
  POSIX does not guarantee atomic writes at this size either, so page checksums
  are needed in any case.
- Less write amplification: updating a 200-byte document rewrites 4 KiB instead
  of 8 KiB.
- Finer buffer pool granularity: less cache memory wasted on cold data under
  random access.
- Fewer documents per page, so less contention if page-level locking is chosen.
- Easier to inspect with a hexdump while debugging.

Cons:

- Lower fanout. With roughly 24-28 byte internal entries (16-byte UUID key,
  child pointer, slot), an internal node holds about 150 entries.
- Smaller documents fit inline. Using the common rule of at least about 4 cells
  per page (SQLite), documents larger than about 1 KiB need overflow pages.

### Option B: 8 KiB

Pros:

- About twice the fanout (about 300 entries per internal node). A 3-level tree
  indexes roughly 27M keys, versus roughly 3.4M with 4 KiB, so fewer I/Os per
  lookup at scale.
- Documents up to about 2 KiB stay inline before overflow.
- The fixed page header is a smaller fraction of the page.
- Fewer I/O calls for sequential scans (partly offset by OS readahead).

Cons:

- One page spans two 4 KiB filesystem or device blocks, so torn writes are more
  likely. PostgreSQL (8 KiB pages) needs `full_page_writes` in its WAL for this
  reason.
- More write amplification and coarser cache granularity.

### Reference

| System                     | Default page size                          |
|----------------------------|--------------------------------------------|
| SQLite                     | 4 KiB                                      |
| PostgreSQL                 | 8 KiB                                      |
| SQL Server                 | 8 KiB                                      |
| InnoDB (MySQL)             | 16 KiB                                     |
| WiredTiger (MongoDB)       | 4 KiB internal pages, up to 32 KiB leaf pages |

## Decision

- Use 4 KiB (4096 bytes) as the default page size.
- The page size is stored in the database file header and read on open. Code
  must use the value from the header, not a hard-coded constant scattered
  across modules. The default lives in one named constant.
- The page size is configurable at database creation time and fixed for the
  lifetime of the file. It must be a power of two between 512 bytes and
  64 KiB, inclusive.

Rationale:

1. Overflow pages are required with either option, because a document can
   exceed 8 KiB. 8 KiB only delays that problem.
2. Aligning with the OS and device block size makes torn-write reasoning
   simpler when the WAL is designed.
3. A configurable page size lets tests use tiny pages (for example 512 bytes),
   so B+Tree splits and merges happen with little data.
4. If benchmarks later show that 8 KiB is better, switching is a header value
   plus the default constant, not a redesign.

## Consequences

- The file header must include a page size field. The exact layout is defined
  in the pager design, together with the magic number and format version.
- Overflow pages must be designed early, since documents above about 1 KiB will
  not fit inline.
- Page checksums are still required. 4 KiB does not guarantee atomic writes.
- Opening a file must validate the stored page size (power of two, within
  bounds) and return a typed error otherwise.
- Benchmarks comparing 4 KiB and 8 KiB should be run once the B+Tree exists,
  before the file format is considered stable.
- Once accepted, this ADR resolves the "Page size" open decision in AGENTS.md
  section 10. AGENTS.md is not edited by this ADR.
