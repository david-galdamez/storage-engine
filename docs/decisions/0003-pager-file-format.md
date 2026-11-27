# 0003. Pager file format

- Status: accepted
- Date: 2026-11-03

## Context

Phase 1 builds the pager: fixed-size pages in one file per database. ADR 0001
sets the page size default to 4096 bytes. The value must be a power of two in
the range [512, 65536], is stored in the file header, and is fixed per file.

The pager needs a binary format for:

- A common page header, present on every page.
- The database file header (page 0).
- Free pages, linked into a free list.
- Slotted pages for variable-size cells, used by the B+Tree in Phase 2.

Constraints:

- No cache yet. The buffer pool is Phase 4.
- No WAL yet. The WAL is Phase 3.
- No `unsafe`.
- No serialization crates. The little-endian codec and the CRC32 are
  hand-written.

## Options considered

### 1. Checksum placement

- Page header (first 4 bytes): the checksum covers one contiguous range,
  `[4, page_size)`. Verification does not need to skip a trailer. Database
  Internals ch. 3 ("Checksumming") places page checksums in the header.
- Page trailer (last 4 bytes): keeps the header at the start of the page, but
  splits the covered bytes around the checksum field and makes the slotted
  layout end at a different offset.

**Chosen:** header.

### 2. Page 0 layout

- Special layout for page 0: the file header replaces the common header.
  Checksum and type handling need a separate path for page 0.
- Same common header on every page: page 0 is the file header with
  `page_type = 1`. One checksum path and one page-type check.

**Chosen:** same common header on every page.

### 3. Reserve the page LSN now or later

- Reserve now (u64, always 0 in Phase 1): the offset is fixed from the start.
  WAL redo (ARIES) compares the pageLSN, so the field is needed in Phase 3.
- Add later: every offset after the common header shifts when the field is
  added, and older files need a format version bump and migration.

**Chosen:** reserve now.

### 4. Free list structure

- Singly linked list of free pages: O(1) allocate and free, simplest to
  implement and to verify.
- SQLite-style trunk pages: each trunk page stores many free page ids. Fewer
  writes on a long list, but more code.
- Bitmap of free pages: compact, but needs a bitmap sized to the file and a
  scan to find free pages.

**Chosen:** linked list, LIFO. Trunk pages can come later with a format version
bump.

### 5. Slot directory

- Tombstones (stable slot ids): a deleted slot stays in the directory, so slot
  ids never change. The directory grows and needs its own reuse rules.
- Positional slots in logical order: the directory is always sorted. Insert and
  remove shift the directory entries. Binary search works directly on the
  slots. Database Internals ch. 4 ("Binary Search with Indirection Pointers")
  describes this layout.

**Chosen:** positional. The B+Tree needs sorted slots for binary search. Deleted
cell bytes are counted in `fragmented_bytes` and reclaimed by compaction. There
is no in-page freeblock list for now.

### 6. Width of `free_end` and `free_start`

- `free_end` as u16: an empty 64 KiB page has `free_end = 65536`, which does not
  fit in a u16.
- `free_end` as u32: fits every page size in range.

**Chosen:** `free_end` is u32. `free_start` is derived as
`32 + 4 * cell_count` and is not stored. Empty cells are rejected, so every cell
offset is at most 65535 and fits in a u16 slot offset.

### 7. I/O API

- Seek and read/write: uses a shared file cursor, so every access moves shared
  state and costs a seek plus a read or write.
- Positioned I/O (`std::os::unix::fs::FileExt::read_exact_at` and
  `write_all_at`): one pread or pwrite syscall per access, no shared cursor,
  and reads take `&self`. Unix-only.

**Chosen:** positioned I/O. Unix-only is acceptable for now.

## Decision

Encoding and addressing:

- All integers are little-endian.
- Offsets are relative to the start of the page.
- `PageId` is a u32. On disk, 0 means "none". Page 0 is the file header. It is
  never free and never referenced as a child.
- Page N lives at file offset `N * page_size`.

Common page header (every page, 16 bytes):

| Offset | Size | Field                 | Notes                                                                                          |
|--------|------|-----------------------|------------------------------------------------------------------------------------------------|
| 0      | 4    | checksum: u32         | CRC32 (IEEE, reflected polynomial 0xEDB88320, init and final XOR 0xFFFFFFFF) of bytes [4, page_size) |
| 4      | 1    | page_type: u8         | 0 = invalid, 1 = FileHeader, 2 = Free, 3 = Slotted                                             |
| 5      | 3    | reserved              | 0                                                                                              |
| 8      | 8    | page_lsn: u64         | reserved for the WAL, 0 in Phase 1                                                             |

File header (page 0, page_type = 1, after the common header):

| Offset | Size | Field                 | Notes                                                                                          |
|--------|------|-----------------------|------------------------------------------------------------------------------------------------|
| 16     | 8    | magic                 | 8 bytes, `STENGDB\0` (ASCII `STENGDB` followed by a zero byte)                                |
| 24     | 2    | format_version: u16   | 1                                                                                              |
| 26     | 2    | reserved              | 0                                                                                              |
| 28     | 4    | page_size: u32        | validated per ADR 0001                                                                         |
| 32     | 4    | page_count: u32       | total pages including page 0, >= 1                                                             |
| 36     | 4    | freelist_head: u32    | 0 = empty, otherwise < page_count                                                              |
| 40     | 4    | freelist_count: u32   | 0 if and only if head is 0; <= page_count - 1                                                  |
| 44     | rest | reserved              | 0 (Phase 2 adds the root/catalog page id with a version bump)                                  |

Free page (page_type = 2):

| Offset | Size | Field                 | Notes                                                                                          |
|--------|------|-----------------------|------------------------------------------------------------------------------------------------|
| 16     | 4    | next_free: u32        | 0 = end of list                                                                                |
| 20     | rest | zero                  |                                                                                                |

Slotted page (page_type = 3):

| Offset      | Size   | Field               | Notes                                                                  |
|-------------|--------|---------------------|------------------------------------------------------------------------|
| 16          | 2      | cell_count: u16     |                                                                        |
| 18          | 2      | fragmented_bytes: u16 | dead cell bytes inside [free_end, page_size)                         |
| 20          | 4      | free_end: u32       | start of the cell area; page_size when empty                           |
| 24          | 1      | flags: u8           | reserved for Phase 2 (leaf/internal), 0                                |
| 25          | 7      | reserved            | Phase 2 (rightmost child / sibling link)                               |
| 32          | 4 * n  | slot directory      | n entries of [offset: u16, len: u16], in logical order                 |
| free_start..free_end | -      | free space          | free_start = 32 + 4 * cell_count                                       |
| free_end..page_size  | -      | cells               | grow backward from the end of the page, may contain holes              |

Slotted page invariants:

1. `32 <= free_start <= free_end <= page_size`.
2. Every slot has `len >= 1`, `offset >= free_end`, and `offset + len <= page_size`.
3. Live cells do not overlap.
4. `page_size - free_end == sum(len) + fragmented_bytes`.
5. Maximum cell length is `page_size - 36` (32-byte header plus one 4-byte slot).
   An insert needs `len + 4` bytes. If the contiguous free space is not enough
   but contiguous plus fragmented space is, the page is compacted first, and
   then the new cell goes at `free_end` like any insert. Otherwise the insert
   fails with a "page full" error and the page is not changed.
6. Compaction rewrites the live cells in logical order from low to high
   addresses: cell 0 starts at the new `free_end`, each next cell starts where
   the previous one ends, and the last cell ends at `page_size`. After it,
   `fragmented_bytes == 0` and the slot order and cell contents are unchanged.
   The result depends only on the cells, so compaction also reorders a page
   with no fragmented bytes. It validates the whole page first and writes
   nothing on a corrupt page. ADR 0004 covers the scratch buffer it uses.
7. Removing a cell zeroes its dead bytes and the freed slot entry, and deletes
   never move `free_end`: only compaction reclaims the space.
8. The free space `[free_start, free_end)` is always all zeros. `init` zeroes
   it, `remove` zeroes the slot it frees, and compaction zeroes the bytes it
   vacates.

Pager behavior:

- The pager stamps the checksum on every write and verifies it on every read.
  Page layouts never touch bytes [0, 4).
- Allocation pops the free list head. If the list is empty, it extends the file
  with a zero-filled page. A page's contents are undefined until its first
  write. Reading it before a write fails the checksum.
- Opening a file: read a 512-byte prefix, check magic, version and page size,
  and require `file length == page_count * page_size`. Then read page 0 fully
  and verify its checksum.
- Freeing a page writes the free page first, then the file header.

## Consequences

- No atomicity across a page write and the file header write. Allocate and free
  are two writes. Until the WAL exists in Phase 3, a crash between them can leak
  a page or leave an inconsistent free list.
- No fsync per write. Durability is explicit through a sync call until Phase 3
  defines the policy.
- Unix-only I/O for now.
- Format version 1. Any layout change bumps `format_version` and gets an ADR.
- Free-list operations cost one extra page write. Trunk pages are a possible
  later optimization.
- Compaction is required, because deletes only increase `fragmented_bytes`.
- Supersedes nothing. Builds on ADR 0001.

## Amendments (2026-11-18)

The implementation in `src/storage/pager/` and the tests in `tests/pager_*.rs`
were checked against the text above. The layouts, the status and the format
version (1) do not change. The items below say where the code is more precise
than this text, or differs from it.

### a. Reading a page before its first write

The text above says that a page's contents are undefined until its first write,
and that reading it fails the checksum. That holds only for pages added by
growing the file: `allocate_page` fills them with zeros, and the checksum check
fails.

A page popped from the free list still holds the free page written when it was
freed. It has type Free and a valid checksum, so reading it succeeds, and
`decode_free_page` returns its `next_free` link. The `allocate_page` doc states
this, and `tests/pager_model.rs` checks it (the `Reused` state).

### b. Open order

`Pager::open` runs these checks in order and returns the first failure:

1. `FileTooSmall` if the file is shorter than 512 bytes.
2. `FileHeader::decode` on the first 512 bytes. It checks, in order: magic
   (`BadMagic`), format version (`UnsupportedVersion`), page size
   (`InvalidPageSize`), page type (`UnknownPageType` or `UnexpectedPageType`),
   and then the header fields (`CorruptHeader`: `page_count` is 0, the free list
   head and count disagree, or the head is past the end of the file). Errors
   from this step are wrapped in `PagerError::Corrupted` with page 0.
3. `FileSizeMismatch` if the file length is not `page_count * page_size`.
4. Page 0 is read in full and its checksum is verified. A failure is
   `Corrupted` with `ChecksumMismatch`.

The checksum is the last check. Damage to the header bytes therefore usually
shows up as `BadMagic`, `CorruptHeader` or another header error, not as a
checksum error. `open` does not walk the free list. `verify_freelist` is a
separate call.

### c. Free list: allocation checks, verification and double free

- `allocate_page` checks the head before it pops it. It reads the head (the
  checksum is verified, as on any read) and requires:
  - the page decodes as a free page, otherwise `FreelistCorrupted`
    (`"page is not a free_page"`);
  - `next_free` is below `page_count`;
  - `next_free` agrees with the new count: it is none exactly when
    `freelist_count - 1` is 0.

  A failed check returns `FreelistCorrupted` with the head page. The file and
  the pager are not changed.
- `Pager::verify_freelist` walks the list from the head, follows at most
  `freelist_count` links, and checks each page. It is the only check of the
  whole list.
- `free_page` does not read the page. Freeing a page twice links it to itself,
  which makes a cycle. `verify_freelist` reports it as a list longer than
  `freelist_count`. `tests/pager_freelist.rs`
  (`verify_detects_a_double_free_without_looping_forever`) covers this.
- Not covered by a test, found by reading the code: after a double free,
  `allocate_page` can return the same page once before the list is reported
  corrupt. The pop checks do not see the cycle, because the head's link points
  to a free page below `page_count` and the count is still above 0. The next
  pop then fails with `FreelistCorrupted`. Until someone calls
  `verify_freelist`, nothing reports the duplicate. A guard in `free_page`
  (refuse a page that is already free) would prevent it, but that check needs
  a read of the page, so it is a decision for the owner.

### d. Growing the file: write order

`allocate_page` grows the file with `set_len` first, and then writes the file
header with the new `page_count`. If the header write fails, the file is one
page longer than the header says. The in-memory `page_count` does not change, so
the process can continue, but the next `Pager::open` fails with
`FileSizeMismatch`. A crash between the two writes has the same effect.

This is a known gap. Revisit it in Phase 3, when the WAL decides the order of
these writes.

### e. Reserved bytes

The rule is: writers write 0, readers ignore. The reserved fields are:

- common header: bytes 5..8, and the page LSN at 8..16 (0 until the WAL exists);
- file header: bytes 26..28 and 44..page_size;
- free page: bytes 20..page_size;
- slotted page: the flags byte at 24 (reserved for Phase 2) and bytes 25..32.

The encoders (`FileHeader::encode`, `encode_free_page` and `SlottedPageMut::init`)
write 0 to all of them, and no decoder checks any of them. The checksum covers
them, because it covers `[4, page_size)`. So a changed reserved byte fails
verification, but only if the page is not stamped again.

`write_page` does not check the bytes the caller passes, so "writers write 0"
is a rule for the layout code, not something the pager enforces.
`tests/pager_disk.rs` (`open_rejects_changed_reserved_byte_in_page_0`) and
`tests/pager_file_header.rs` (`decode_ignores_checksum_lsn_and_reserved_bytes`)
cover this.

## Amendment (2026-11-27): page LSN and the write-ahead log

ADR 0006 uses the page LSN reserved by option 3: `write_page` stamps the LSN
of the current WAL operation, and 0 still means "written before the WAL". The
reserved-bytes rule of amendment e no longer covers bytes 8..16 of the common
header. ADR 0006 also closes the gap of amendment d and the crash items of the
Consequences: allocation, freeing and file growth become part of one logged
operation, and the file length changes only after the log is durable.
