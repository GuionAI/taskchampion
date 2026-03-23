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

## iOS (Swift Package Manager)

The `ffi/` crate provides a UniFFI-based FFI layer for iOS consumption via SPM.

### Building

```bash
# Install Rust iOS targets + build XCFramework + generate Swift bindings
./scripts/build_xcframework.sh
```

This produces:
- `TaskChampionFFIFFI.xcframework/` — static library for iOS device + simulator
- `Sources/TaskChampionFFI/TaskChampionFFI.swift` — generated Swift bindings

### Consuming from an iOS Project

1. Add this repo as a git submodule:
   ```bash
   git submodule add https://github.com/tta-lab/taskchampion.git vendor/taskchampion
   ```

2. Run the build script:
   ```bash
   cd vendor/taskchampion && ./scripts/build_xcframework.sh
   ```

3. In Xcode: **Add Local Package** → select `vendor/taskchampion/` → add `TaskChampionFFI` to your target.

4. Import and use:
   ```swift
   import TaskChampionFFI

   // Inside a PowerSync writeTransaction:
   let handle = Int64(Int(bitPattern: tx.pointer))
   let tasks = try pendingTasks(dbHandle: handle, userId: userId)
   ```
