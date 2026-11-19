# 0002. Test dev-dependencies

- Status: accepted
- Date: 2026-11-03

## Context

The Phase 1 pager needs two kinds of tests that the standard library does not
cover well:

- Property-based tests. AGENTS.md section 6 requires `proptest` for
  encoders/decoders, page layouts, and B+Tree invariants.
- File-based integration tests that create, reopen, and corrupt database files
  using temporary files.

AGENTS.md section 7 requires an ADR for every new dependency.

## Options considered

### Property-based testing

- `proptest`: shrinking of failing inputs, composable strategies, and
  regression files that pin past failures. It is the crate AGENTS.md names.
- `quickcheck`: simpler API, but weaker shrinking and weaker strategy
  composition.
- Hand-rolled random tests: no dependency, but no shrinking, so failures are
  hard to minimize and reproduce.

### Temporary files

- `tempfile`: unique names per call, and automatic cleanup on drop, including
  when a test fails.
- Manual paths under `std::env::temp_dir`: name collisions between tests that
  run in parallel, and no cleanup when a test fails.

## Decision

- Add `proptest = "1"` and `tempfile = "3"` as `[dev-dependencies]` only.
- Neither crate is ever added to the library's runtime `[dependencies]`.

## Consequences

- Test builds take longer because of the extra crates.
- No effect on the shipped binary or library.
- `proptest` writes a `tests/<test file>.proptest-regressions` file next to
  each integration test file that has a failing property (for example
  `tests/pager_codec.proptest-regressions`). These files should be committed
  when they appear.

Amendment (2026-11-18): this section first said that the files go in
`proptest-regressions/`. That was wrong. The files sit next to the test file,
as described above.
