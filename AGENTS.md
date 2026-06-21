# AGENTS.md

This file provides guidance to coding agents when working with code in this repository.

## What is TaskChampion

TaskChampion is the Rust library that implements task storage behind [Taskwarrior](https://taskwarrior.org/). This repo contains the core `taskchampion` crate (root) and a private `xtask` crate for MSRV management. This is a PowerSync-only fork — sync is handled externally by PowerSync, not by this library.

## Build & Test Commands

```bash
cargo build                    # build with default features
cargo test                     # run all tests
cargo test -p taskchampion     # run only taskchampion crate tests
cargo test <test_name>         # run a single test by name

# Linting
cargo fmt --all -- --check     # format check
cargo clippy --features storage-powersync --no-deps -- -D warnings

# Docs
cargo doc --release --open -p taskchampion
```

MSRV: **1.91.1** (update with `cargo xtask msrv <version>`)

## SQLx Metadata

The pgwire backend uses SQLx compile-time query macros. After changing any
`sqlx::query!`, `query_as!`, or `query_scalar!` call, run
`./scripts/sqlx-prepare.sh` and commit the `.sqlx/` changes.

The prepare database must have the matching FlickNote backend migrations
applied first. For short task IDs, this means the backend
`add_user_short_ids` migration must exist locally before regenerating metadata.
Treat missing-column prepare failures as stale schema, not as a reason to
hand-edit `.sqlx/`.

## Architecture

### Core Abstractions

- **`Replica`** (`src/replica.rs`) — Main entry point. Represents one instance of a user's task data. Wraps a `TaskDb` and provides high-level task CRUD and undo.
- **`TaskDb`** (`src/taskdb/`) — Applies operations to storage and manages undo. Not public API; used internally by `Replica`.
- **`Operation`/`Operations`** (`src/operation.rs`) — All mutations are expressed as operations that get committed atomically via `Replica::commit_operations(ops)`.
- **`Task`/`TaskData`** (`src/task/`) — `TaskData` is the low-level key-value map wrapper; `Task` is the high-level type with typed property accessors, dependency management, and tag support.
- **`DependencyMap`** (`src/depmap.rs`) — Tracks task dependency relationships.

### Storage Layer (`src/storage/`)

Trait-based: `Storage` creates `StorageTxn` (transaction) objects. All storage access is async and transactional.

Backends:
- `storage-powersync` — PowerSync-compatible SQLite (default, feature-gated)
- `inmemory` — In-memory (always available, used in tests)

### Server Module (`src/server/`)

Contains only `VersionId`/`NIL_VERSION_ID` types (used by `StorageTxn`) and `SyncOp` (used by `taskdb/apply.rs` for operation application logic).

## Key Patterns

- **Async throughout**: All storage traits use `async_trait`. Tests use `#[tokio::test]`.
- **Feature flags**: Default features are `storage-powersync` and `bundled`.
- **Strict lints**: `clippy::all`, `unreachable_pub`, `unnameable_types`, `clippy::dbg_macro` are all denied in `lib.rs`.
- **Generated FFI bindings**: Do not hand-generate or commit changes to `Sources/TaskChampionFFI/TaskChampionFFI.swift` or `Sources/TaskChampionFFI/TaskChampionFFIFFI.h`. CI generates these bindings.
