This crate implements the core of TaskChampion, the [replica](crate::Replica).

Users of this crate can manipulate a task database using this API.

Example uses of this crate:
 * user interfaces for task management, such as mobile apps, web apps, or command-line interfaces
 * integrations for task management, such as synchronization with ticket-tracking systems or
   request forms.

# Replica

A TaskChampion replica is a local copy of a user's task data.

Replicas are accessed using the [`Replica`] type.

# Task Storage

Replicas access the task database via a [storage object](crate::storage::Storage).

The [`storage`] module supports pluggable storage for a replica's data.
An implementation is provided, but users of this crate can provide their own implementation as well.

# Example

```rust
# use taskchampion::chrono::Utc;
# use taskchampion::{Operations, Replica, Status, Uuid};
# use taskchampion::storage::inmemory::InMemoryStorage;
# #[tokio::main(flavor = "current_thread")]
# async fn main() -> anyhow::Result<()> {
# let mut replica = Replica::new(InMemoryStorage::new());
// Create a new task, recording the required operations.
let mut ops = Operations::new();
let uuid = Uuid::new_v4();
let mut t = replica.create_task(uuid, &mut ops).await?;
t.set_description("my first task".into(), &mut ops)?;
t.set_status(Status::Pending, &mut ops)?;
t.set_entry(Some(Utc::now()), &mut ops)?;

// Commit those operations to storage.
replica.commit_operations(ops).await?;
#
# Ok(())
# }
```

# Feature Flags

Support for some optional functionality is controlled by feature flags.

 * `storage-powersync` - store task data using PowerSync-compatible SQLite
 * `bundled` - activates bundling system libraries like sqlite
 * `test-utils` - expose `*_for_test()` constructors for use in external crate tests

By default, `storage-powersync` and `bundled` are enabled.

# Minimum Supported Rust Version (MSRV)

This crate supports Rust version 1.91.1 and higher.
