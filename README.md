# storage-engine-rs *(working name)*

A document-oriented database written from scratch in Rust, built as a learning
project while studying *Database Internals* (Alex Petrov).

It starts as a simple key-value storage engine and grows step by step into a
full document database: MongoDB-style JSON-like documents, collections with
validation, secondary indexes, ACID transactions, a binary TCP protocol, and
eventually vector tables for similarity search.

> **Status:** early development (Phase 0). Nothing here is production-ready.
> The project has no final name yet.

---

## Goals

- **Learn by building.** Every component (pager, B+Tree, WAL, buffer pool,
  query engine...) is implemented by hand to understand how real databases work.
- **Fast reads.** A B+Tree storage engine and a binary document format designed
  so that fields can be read in place, without deserializing the whole document.
- **Zero-copy where possible.** The code favors references and slices over
  owned copies. Any exception is studied and documented.
- **Safe by default.** No panics in library code (enforced by strict Clippy
  lints). `unsafe` is allowed only with a documented justification.
- **Open to any client.** A documented binary wire protocol so that drivers can
  be written in any language.

## Concepts

```
Server
 └── Database            (one data file per database, fixed-size pages)
      ├── Model           (equivalent to a MongoDB collection, with optional validation rules)
      │    ├── Document   (JSON-like, binary encoded, identified by a UUID `_id`)
      │    └── Indexes    (single-field, compound, unique)
      └── Vector table    (future: vectors + approximate nearest-neighbor index)
```

### Document types

| Type      | Description                         |
|-----------|-------------------------------------|
| `string`  | UTF-8 text                          |
| `int64`   | 64-bit signed integer               |
| `float64` | 64-bit IEEE 754 floating point      |
| `bool`    | `true` / `false`                    |
| `null`    | Absence of value                    |
| `array`   | Ordered list of values              |
| `object`  | Nested document                     |
| `binary`  | Raw bytes                           |
| `date`    | Timestamp                           |
| `uuid`    | 128-bit identifier (used for `_id`) |

### Queries

Queries use JSON filters, similar to MongoDB. This makes client libraries easy
to write:

```json
{ "age": { "$gt": 18 }, "status": "active" }
```

Aggregations (`$match`, `$group`, `$sort`, `$project`, ...) are in scope.

## Architecture

```
 ┌──────────────┐      binary protocol over TCP      ┌──────────────────────────┐
 │ Client / CLI │ ─────────────────────────────────▶ │ Server (tokio)           │
 └──────────────┘                                    │  ├─ Protocol / parser    │
                                                     │  ├─ Query engine         │
                                                     │  ├─ Transactions (ACID)  │
                                                     │  ├─ Catalog & validation │
                                                     │  ├─ Indexes              │
                                                     │  ├─ Document format      │
                                                     │  └─ Storage engine       │
                                                     │      ├─ B+Tree           │
                                                     │      ├─ Buffer pool      │
                                                     │      ├─ WAL & recovery   │
                                                     │      └─ Pager (disk)     │
                                                     └──────────────────────────┘
```

The storage engine sits behind a `StorageEngine` trait, so other
implementations (for example an LSM-Tree) can be tried later.

## Roadmap

| Phase | Milestone                                                                 | Status      |
|-------|---------------------------------------------------------------------------|-------------|
| 0     | In-memory key-value store + REPL to test it                               | In progress |
| 1     | Pager: on-disk file, fixed-size pages, slotted page layout                | Planned     |
| 2     | B+Tree key-value storage engine behind a `StorageEngine` trait            | Planned     |
| 3     | Write-ahead log (WAL) and crash recovery                                  | Planned     |
| 4     | Buffer pool / page cache                                                  | Planned     |
| 5     | Binary document format with in-place field access, UUID `_id`             | Planned     |
| 6     | Databases, models (collections), catalog and per-model validation         | Planned     |
| 7     | Query engine: JSON filters and projections                                | Planned     |
| 8     | Secondary indexes: single-field, compound, unique                         | Planned     |
| 9     | ACID transactions (concurrency control: MVCC vs. locking, to be decided)  | Planned     |
| 10    | Aggregation pipeline                                                      | Planned     |
| 11    | Async TCP server (tokio), binary wire protocol, separate client binary    | Planned     |
| 12    | Vector tables with an approximate nearest-neighbor index (e.g. HNSW)      | Future      |

## Getting started

Requirements: a recent stable Rust toolchain (edition 2024).

```bash
cargo build
cargo run          # starts the test REPL
```

REPL commands (Phase 0, in-memory only):

```
$ set <key> <value>
$ get <key>
$ delete <key>
$ scan
$ exit
```

The REPL exists only to check that data is stored correctly. It will be
replaced by two binaries: a **server** and a **client**.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three must pass before every commit, and CI runs them on every push.
See [AGENTS.md](AGENTS.md) for the development workflow and coding rules.

## References

- Alex Petrov, *Database Internals* (O'Reilly, 2019)
- MongoDB documentation: data model, query and aggregation semantics
- SQLite file format documentation: pages and B-Trees on disk

## License

To be decided.
