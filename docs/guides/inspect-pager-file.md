# Inspecting a pager file with `xxd`

This guide walks through a real database file written by the pager and
explains every byte that matters. The layouts come from
[ADR 0003](../decisions/0003-pager-file-format.md); the compaction layout is
explained further in [ADR 0004](../decisions/0004-compaction-scratch-buffer.md).
The offsets below were checked against the code (`src/storage/pager/`), not only
the ADR.

## Run the example

```bash
cargo run --example pager_demo -- --force
```

Options: `[PATH]` (default `target/pager-demo.db`), `--page-size N` (default
512, a power of two from 512 to 65536) and `--force` (delete `PATH` first; the
pager refuses to overwrite an existing file).

The file has five 512-byte pages:

| Page | File offset | What it holds |
|------|-------------|---------------|
| 0    | `0x000`     | file header: `page_count` 5, `freelist_head` 4, `freelist_count` 2 |
| 1    | `0x200`     | slotted page with `apple`, `banana`; `cherry` was removed, leaving a hole |
| 2    | `0x400`     | the same page after `compact()`: no hole, `banana` moved |
| 3    | `0x600`     | free page, `next_free` = 0 (end of list) |
| 4    | `0x800`     | free page, `next_free` = 3 |

Pages 3 and 4 were allocated and then freed in the order 3, 4. The free list is
LIFO: head 4 -> 3 -> none.

Then dump the file:

```bash
xxd -a target/pager-demo.db                       # whole file
xxd -s 0x200 -l 512 target/pager-demo.db          # one page
```

`-a` collapses runs of zero lines into a single `*`. All numbers are stored
**little-endian**: the least significant byte comes first. The four bytes
`00 02 00 00` are the u32 `0x00000200` = 512, not `0x00020000`.

## The common header (every page, bytes 0-15)

| Offset | Size | Field |
|--------|------|-------|
| 0      | 4    | checksum (u32) |
| 4      | 1    | page type: 1 = file header, 2 = free, 3 = slotted |
| 5      | 3    | reserved, 0 |
| 8      | 8    | page LSN (u64), reserved for the WAL, 0 for now |

The checksum is a CRC32 (IEEE) of bytes `[4, page_size)`: everything in the page
**except** the checksum field itself. The pager stamps it on every write and
checks it on every read. For page 0 below, the bytes `7b ac 64 19` are the
little-endian u32 `0x1964ac7b`, and the CRC32 of the other 508 bytes of page 0
is exactly that value. (The checksum of page 0 changed when the header moved to
format version 2.)

## Page 0: the file header (offset `0x000`)

The file header layout is format version 2. It adds `catalog_root` at offset 44
and moves the reserved bytes to offset 48. See
[ADR 0005](../decisions/0005-btree-node-format.md) (section "File header, format
version 2").

```
00000000: 7bac 6419 0100 0000 0000 0000 0000 0000  {.d.............
00000010: 5354 454e 4744 4200 0200 0000 0002 0000  STENGDB.........
00000020: 0500 0000 0400 0000 0200 0000 0000 0000  ................
00000030: 0000 0000 0000 0000 0000 0000 0000 0000  ................
*
```

| Bytes (hex)               | Offset | Field |
|---------------------------|--------|-------|
| `7b ac 64 19`             | 0      | checksum = `0x1964ac7b` |
| `01`                      | 4      | page type 1 = file header |
| `00 00 00`                | 5      | reserved |
| `00 00 00 00 00 00 00 00` | 8      | page LSN = 0 |
| `53 54 45 4e 47 44 42 00` | 16     | magic `STENGDB\0` |
| `02 00`                   | 24     | format version = 2 |
| `00 00`                   | 26     | reserved |
| `00 02 00 00`             | 28     | page size = `0x200` = 512 |
| `05 00 00 00`             | 32     | page count = 5 (including page 0) |
| `04 00 00 00`             | 36     | free list head = page 4 |
| `02 00 00 00`             | 40     | free list count = 2 |
| `00 00 00 00`             | 44     | catalog root = 0 (none yet) |
| zeros                     | 48-511 | reserved |

## Page 1: slotted page with a hole (offset `0x200`)

The words were inserted in the order `cherry`, `apple`, `banana`, each at its
sorted position, then `cherry` was removed. Cells grow backward from the end of
the page, so the physical order is the reverse of the insert order, while the
slot directory keeps the logical (sorted) order.

```
00000200: 21d6 3d30 0300 0000 0000 0000 0000 0000  !.=0............
00000210: 0200 0600 ef01 0000 0000 0000 0000 0000  ................
00000220: f501 0500 ef01 0600 0000 0000 0000 0000  ................
00000230: 0000 0000 0000 0000 0000 0000 0000 0000  ................
*
000003e0: 0000 0000 0000 0000 0000 0000 0000 0062  ...............b
000003f0: 616e 616e 6161 7070 6c65 0000 0000 0000  ananaapple......
```

Offsets are relative to the start of the page (add `0x200` for the file
offset).

| Bytes (hex)     | Offset | Field |
|-----------------|--------|-------|
| `21 d6 3d 30`   | 0      | checksum |
| `03`            | 4      | page type 3 = slotted |
| `02 00`         | 16     | cell count = 2 |
| `06 00`         | 18     | fragmented bytes = 6 (the dead `cherry`) |
| `ef 01 00 00`   | 20     | free_end = `0x1ef` = 495, where the cell area starts (u32) |
| `00`            | 24     | flags, reserved |
| zeros           | 25-31  | reserved |
| `f5 01` `05 00` | 32     | slot 0: offset 501, length 5 -> `apple` |
| `ef 01` `06 00` | 36     | slot 1: offset 495, length 6 -> `banana` |
| zeros           | 40-494 | free space: from `free_start` = 32 + 4 * 2 = 40 up to `free_end` |
| `banana`        | 495    | cell, file offset `0x3ef` |
| `apple`         | 501    | cell, file offset `0x3f5` |
| `00` x 6        | 506    | the hole left by `cherry`, zeroed (counted in fragmented bytes) |

The invariant `page_size - free_end == sum(len) + fragmented_bytes` holds:
`512 - 495 = 17 = 6 + 5 + 6`. Removing a cell never moves `free_end`; only
compaction reclaims the hole.

## Page 2: after compaction (offset `0x400`)

Same steps, then `compact()`. Live cells are rewritten in logical order from low
to high addresses, ending at the end of the page.

```
00000400: 6dde 0206 0300 0000 0000 0000 0000 0000  m...............
00000410: 0200 0000 f501 0000 0000 0000 0000 0000  ................
00000420: f501 0500 fa01 0600 0000 0000 0000 0000  ................
00000430: 0000 0000 0000 0000 0000 0000 0000 0000  ................
*
000005f0: 0000 0000 0061 7070 6c65 6261 6e61 6e61  .....applebanana
```

| Bytes (hex)     | Offset | Field |
|-----------------|--------|-------|
| `6d de 02 06`   | 0      | checksum |
| `02 00`         | 16     | cell count = 2 |
| `00 00`         | 18     | fragmented bytes = 0 |
| `f5 01 00 00`   | 20     | free_end = 501 |
| `f5 01` `05 00` | 32     | slot 0: offset 501, length 5 -> `apple` |
| `fa 01` `06 00` | 36     | slot 1: offset 506, length 6 -> `banana` |

Compare with page 1: `banana` moved from 495 to 506, `free_end` went from 495
to 501, and the hole is gone. Slot order and cell contents did not change.

## Pages 3 and 4: free pages (offsets `0x600` and `0x800`)

```
00000600: 57e2 d25b 0200 0000 0000 0000 0000 0000  W..[............
00000610: 0000 0000 0000 0000 0000 0000 0000 0000  ................
*
00000800: 4192 4011 0200 0000 0000 0000 0000 0000  A.@.............
00000810: 0300 0000 0000 0000 0000 0000 0000 0000  ................
```

| Bytes (hex)   | Offset | Field |
|---------------|--------|-------|
| checksum      | 0      | `57 e2 d2 5b` (page 3), `41 92 40 11` (page 4) |
| `02`          | 4      | page type 2 = free |
| `00 00 00 00` | 16     | `next_free`: page 3 -> 0 (end of list) |
| `03 00 00 00` | 16     | `next_free`: page 4 -> 3 |
| zeros         | 20-511 | unused |

The chain starts at the file header (offset 36, head = 4): page 4 -> page 3 ->
none. A `next_free` of 0 means "none", because page 0 is never free.

## Other page sizes

`--page-size 4096` (the default page size of the engine, ADR 0001) changes
only the numbers that depend on the size:

- Pages start at multiples of `0x1000`: page 1 at `0x1000`, page 4 at `0x4000`.
- The file header stores `00 10 00 00` at offset 28 (0x1000 = 4096).
- `free_end` of an empty slotted page is 4096, and the cells sit at the end of
  the page: here page 1 has `free_end` = 4079 (`ef 0f`) and `apple` at 4085.
- The checksum covers `[4, 4096)`, so every checksum differs.
- The common header, file header and slotted header offsets are the same.

Sizes 1024 and 65536 also work. At 65536 an empty page has `free_end` = 65536,
which is why `free_end` is a u32 (ADR 0003, decision 6). Sizes that are not a
power of two in `[512, 65536]` are rejected.
