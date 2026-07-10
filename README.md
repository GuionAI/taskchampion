TaskChampion
------------

TaskChampion implements the task storage and synchronization behind Taskwarrior.
It includes an implementation with Rust and C APIs, allowing any application to maintain and manipulate its own replica.
It also includes a specification for tasks and how they are synchronized, inviting alternative implementations of replicas or task servers.

See the [documentation](https://gothenburgbitfactory.org/taskchampion/) for more!

## Structure

There are two crates here:

 * `taskchampion` (root of the repository) - the core of the tool
 * [`xtask`](./xtask) (private) - implementation of the `cargo xtask msrv` command

## Rust API

The Rust API, as defined in [the docs](https://docs.rs/taskchampion/latest/taskchampion/), supports simple creation and manipulation of replicas and the tasks they contain.

The Rust API follows semantic versioning.

## SQLx query metadata

The pgwire storage backend uses SQLx compile-time query checks. CI verifies those
queries in offline mode using the committed `.sqlx/` metadata.

After changing a `sqlx::query!`, `sqlx::query_as!`, or `sqlx::query_scalar!` call,
run this against a local backend schema and commit the updated `.sqlx/` files:

```bash
./scripts/sqlx-prepare.sh
```

Set `SQLX_POSTGRES_DATABASE_URL` if your local database is not available at the
script default URL.

## iOS & macOS (Swift Package Manager)

The `ffi/` crate provides a UniFFI-based FFI layer for iOS and macOS consumption via SPM.

### Migration note

The public SwiftPM product remains `TaskChampionFFI`, so app code should still
use `import TaskChampionFFI`. The low-level binary target and release asset were
renamed from the legacy doubled-FFI name (`TaskChampionFFIFFI`) to
`TaskChampionCore`. Consumers that only add the `TaskChampionFFI` product do not
need code changes; consumers that referenced the binary target or release zip
directly should update those references to `TaskChampionCore.xcframework.zip`.

### Building

```bash
# macOS only: install cargo-swift once
cargo install cargo-swift@0.11.1 --locked

# Build the default static Swift package and TaskChampionCore XCFramework
./scripts/package_cargo_swift.sh

# Dynamic tags/releases are built with the same package shape:
TASKCHAMPION_FFI_LINKAGE=dynamic ./scripts/package_cargo_swift.sh target/cargo-swift-dynamic
```

This produces:
- `target/cargo-swift/TaskChampionFFI/TaskChampionCore.xcframework/` — default static framework for iOS device, iOS simulator, and macOS
- `target/cargo-swift-dynamic/TaskChampionFFI/TaskChampionCore.xcframework/` — dynamic framework when requested
- `target/cargo-swift/TaskChampionFFI/Sources/TaskChampionFFI/taskchampion_ffi.swift` — generated Swift bindings

For local Xcode testing, point `Package.swift` at the built XCFramework instead
of the release zip:

```bash
./scripts/use_local_xcframework.sh target/cargo-swift/TaskChampionFFI/TaskChampionCore.xcframework
```

Restore the release URL before committing release changes:

```bash
git restore Package.swift
```

### Consuming from an iOS or macOS Project

1. Add this repo as a git submodule:
   ```bash
   git submodule add https://github.com/GuionAI/taskchampion.git vendor/taskchampion
   ```

2. Run the build script:
   ```bash
   cd vendor/taskchampion
   cargo install cargo-swift@0.11.1 --locked
   ./scripts/package_cargo_swift.sh
   ./scripts/use_local_xcframework.sh target/cargo-swift/TaskChampionFFI/TaskChampionCore.xcframework
   ```

3. In Xcode: **Add Local Package** → select `vendor/taskchampion/` → add `TaskChampionFFI` to your target.

4. Import and use:
   ```swift
   import TaskChampionFFI

   // Create a session once at login/startup
   let session = try FfiSession(executor: myExecutor, userId: userId)

   // All task operations are async
   let tasks = try await session.pendingTasks()
   let created = try await session.createTask(uuid: UUID().uuidString, description: "New task")
   ```
