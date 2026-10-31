# AGENTS.md

Guidelines for AI agents (Claude Code and others) working on this repository.
Read this whole file before doing any work.

## 1. Project context

This is a document database written from scratch in Rust (see
[README.md](README.md)). It starts as a key-value storage engine and grows into
a MongoDB-style document database with models (collections), validation,
indexes, ACID transactions, a binary TCP protocol, and later vector tables.

**The owner is learning.** They are reading *Database Internals* and want to
**write the database themselves**. Agents are mentors and assistants, not the
main authors. See section 3.

## 2. Language

Everything is in **English**: code, identifiers, comments, docs, commit
messages, ADRs, and PR descriptions. (Chat with the owner can be in Spanish.)

## 3. Ownership: who writes what

### Core database code: the owner writes it

These areas are written by the owner:

- Storage engine: pager, page layout, B+Tree, buffer pool, WAL, recovery
- Document binary format and encoding/decoding
- Catalog, models, and validation
- Query engine, filters, aggregations
- Indexes
- Transactions and concurrency control
- Wire protocol and server internals
- Vector tables and the ANN index

For these areas, agents must:

- Explain concepts, with references to *Database Internals* chapters when
  possible.
- Propose designs, trade-offs, and step-by-step plans.
- Give hints, pseudocode, or small illustrative snippets **when asked**.
- Review the owner's code: correctness, borrowing, safety, performance.
- Write tests **if the owner asks**.

Agents **must not** write the implementation of core components unless the
owner asks for it explicitly in that conversation. When in doubt, ask.

### Supporting code: agents may write it (after plan approval)

- CI configuration, tooling, scripts
- Test harnesses, benchmarks, fixtures
- The throwaway test REPL and CLI plumbing
- Documentation

## 4. Agent workflow

Work is always done through **sub-agents**, each with a fixed role:

| Step | Role                       | Model      | Output                                    |
|------|----------------------------|------------|-------------------------------------------|
| 1    | Planning / design          | **Opus**   | Written plan: files, steps, trade-offs    |
| 2    | **Owner approves the plan**| Owner      | Approved, changed, or rejected plan       |
| 3    | Implementation             | **Sonnet** | Code changes that follow the approved plan|
| 4    | Verification               | Sonnet     | `fmt`, `clippy`, and `test` all passing   |
| 5    | Documentation + commit msg | **Haiku**  | Updated docs and a proposed commit message|
| 6    | **Owner approves commit**  | Owner      | Commit is created only after approval     |

Rules:

- **Never start implementation without an approved plan.**
- **Never commit, push, or open a PR without the owner's explicit approval.**
  Show the diff summary and the proposed message, then wait.
- If the implementation must leave the approved plan, stop and go back to step 2.
- For core components (section 3), step 3 usually means *the owner* writes
  the code and Sonnet reviews it.

## 5. Rust rules

### 5.1 References and slices first

Always prefer borrowing: `&T`, `&mut T`, `&[T]`, `&str`, and lifetimes on
structs that view into buffers (for example a `DocumentRef<'a>` over a page
slice).

When borrowing seems impossible, **study the case before giving up**. Try, in
this order:

1. Restructure ownership or scope (split borrows, shorter borrows, reorder code).
2. Explicit lifetimes and borrowed views into existing buffers.
3. Indices or offsets into a buffer instead of references.
4. `Cow<'a, T>` when only some paths need ownership.
5. Arena or slab allocation.
6. Only then: owned data (`clone`, `to_owned`, `String`, `Vec`) or shared
   ownership (`Rc`/`Arc`).

If step 6 is needed, **write an ADR** (section 7) explaining which alternatives
were tried and why they failed. Get the owner's approval before merging.

### 5.2 No panics

`Cargo.toml` enforces strict Clippy lints (`pedantic`, `nursery`, and denies
`unwrap`, `expect`, `panic`, indexing, unchecked arithmetic, `as` casts, and more).

- Use `Result` and typed errors (`thiserror`) for library code. `anyhow` is
  only for binaries.
- Use `.get()`, `checked_*`/`saturating_*`/`wrapping_*`, `try_from`/`try_into`.
- Never weaken lints in `Cargo.toml`.
- `#[allow]` is forbidden. If a lint must be suppressed, use
  `#[expect(lint, reason = "...")]` at the narrowest scope, and only with the
  owner's approval.

### 5.3 `unsafe`

`unsafe` is allowed **only** with:

1. An ADR justifying why safe code is not enough (performance, mmap, aligned
   buffers, etc.).
2. The owner's explicit approval.
3. A `// SAFETY:` comment on every `unsafe` block, explaining the invariants.
4. Tests that exercise the unsafe code paths.

Keep `unsafe` in small, isolated modules with safe public APIs.

### 5.4 Style

- `cargo fmt` formatting.
- Small modules with clear responsibilities, following the layers in the README.
- Public items have doc comments (`///`).
- Async code uses `tokio`. The storage layer stays runtime-agnostic where
  practical.

## 6. Tests and CI

Tests are mandatory, and all checks must pass before every commit:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

- Unit tests next to the code (`#[cfg(test)]`), integration tests in `tests/`.
- Property-based tests (`proptest`) for encoders/decoders, page layouts, and
  B+Tree invariants.
- Crash/recovery tests for the WAL once it exists.
- The CI pipeline in the git repository runs the same checks. A red CI is never
  merged.

## 7. Architecture Decision Records (ADRs)

Significant decisions go in `docs/decisions/NNNN-short-title.md`:

```markdown
# NNNN. Title

- Status: proposed | accepted | superseded by NNNN
- Date: YYYY-MM-DD

## Context
## Options considered
## Decision
## Consequences
```

ADRs are required for: owned data instead of references (5.1), any `unsafe`
(5.3), lint suppressions, new dependencies, and on-disk or wire format changes.

## 8. Commits

- [Conventional Commits](https://www.conventionalcommits.org/): `feat`, `fix`,
  `refactor`, `docs`, `test`, `perf`, `chore`, `ci`.
- One logical change per commit, with an imperative subject of 72 characters or less.
- Haiku drafts the message; the owner approves it before the commit is created.

## 9. Decisions already made

| Topic                 | Decision                                              |
|-----------------------|-------------------------------------------------------|
| Storage structure     | B+Tree, behind a `StorageEngine` trait                |
| Files                 | One file per database, fixed-size pages               |
| Durability            | WAL + crash recovery                                  |
| Data model            | Database → Models (collections) → Documents           |
| Validation            | Per-model validation rules                            |
| Document format       | Custom binary format, in-place field access           |
| Document `_id`        | UUID                                                  |
| Query language        | JSON filters (MongoDB-style), aggregations in scope   |
| Indexes               | Single-field, compound, unique, early on              |
| Transactions          | ACID, multi-document                                  |
| Networking            | Async `tokio`, custom binary protocol                 |
| Binaries              | Separate server and client (the REPL is temporary)    |
| Vectors               | Dedicated vector tables (future)                      |

## 10. Open decisions

- Concurrency control: MVCC vs. locking (page-level or collection-level).
- Page size (4 KiB vs. 8 KiB).
- Project name and license.
