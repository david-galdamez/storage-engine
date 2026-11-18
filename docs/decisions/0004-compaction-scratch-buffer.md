# 0004. Owned scratch buffer for slotted page compaction

- Status: accepted
- Date: 2026-11-18

## Context

Compaction (Phase 1, step 9) reclaims the bytes counted in `fragmented_bytes`.
ADR 0003 (invariant 6) fixes the layout it produces: the live cells are
rewritten in logical order from low to high addresses. Cell 0 starts at the new
`free_end`, each next cell starts where the previous one ends, and the last cell
ends at `page_size`. This layout was chosen because:

- The result depends only on the cells, not on the history of the page. The
  same cells always give the same bytes, which makes tests, `xxd` dumps and
  debugging simpler.
- Right after compaction, a range of logical cells is one contiguous block of
  bytes. A B+Tree split (Phase 2) can move the upper half of a page with one
  copy, as long as no insert happened since the last compaction.
- The algorithm is one pass over the slots: O(n + live bytes).

To write a cell at its target offset, compaction may overwrite another live cell
that it has not moved yet. Example: hello (cell 0) at 507..512 and ab (cell 1)
at 505..507 must become hello at 505..510 and ab at 510..512. Writing hello
first destroys ab, and writing ab first destroys hello. So the live bytes must
be saved somewhere before the page is rewritten.

AGENTS.md section 5.1 requires an ADR before using owned data, listing the
alternatives that were tried.

## Options considered

Following the order of AGENTS.md section 5.1:

1. **Restructure in place.** Sliding each cell toward the page end in
   physical order works with `copy_within` and needs no buffer, because every
   cell only moves to higher addresses. But it keeps the physical order, not
   the logical order, and it is O(n²) without sorting the slots by offset
   (sorting needs a buffer too). An in-place permutation into logical order is
   not practical: cells have different lengths, so a target range can cover
   parts of several cells, and there is no simple cycle to follow.
2. **Borrowed views.** Slices of the cells borrow the page, and the rewrite
   needs `&mut` to the same page. The borrow checker rejects it, and with
   reason: the bytes behind the slices are overwritten.
3. **Indices or offsets.** They say where each cell is, but the bytes still
   need to live somewhere while their old location is overwritten.
4. **`Cow`.** Every compaction needs the copy, so there is no borrowed path to
   keep.
5. **Arena or caller-provided scratch.** `compact(&mut self, scratch: &mut
   [u8])` avoids allocation. But `insert` can compact, so it would need the
   scratch parameter too, and every caller would carry a buffer that is used
   rarely. Nothing owns a reusable page buffer yet; the buffer pool comes
   later. A stack array is not an option: the page size is only known at
   runtime, up to 64 KiB, and Clippy denies `large_stack_arrays`.
6. **Owned `Vec<u8>`.** One allocation per compaction, at most
   `page_size - 36` bytes.

## Decision

`SlottedPageMut::compact` copies the live cells, in logical order, into a
`Vec<u8>` allocated inside the function, with capacity equal to the sum of the
live cell lengths. It then writes that buffer to `[page_size - live, page_size)`,
updates every slot offset, zeroes `[free_start, new free_end)`, sets
`fragmented_bytes` to 0 and `free_end` to `page_size - live`.

The page is validated before the copy. A corrupt page fails with
`CorruptSlottedPage` and is not written. The public API stays `compact()` with
no parameters, and `insert` keeps its signature.

## Consequences

- One heap allocation per compaction. Compaction only runs when an insert does
  not fit in the contiguous free space, or when a caller asks for it, so this
  is rare compared to inserts and reads.
- If the allocation fails, the process aborts, as with any `Vec` in the
  standard library. The buffer is at most one page, so this is accepted.
- No format change and no `format_version` bump. Readers accept cells in any
  physical order; only what compaction writes is fixed.
- To revisit when the buffer pool exists: it can lend a reusable scratch page,
  and `compact` can take it as a parameter. That change stays inside the crate.
- Builds on ADR 0003. Supersedes nothing.
