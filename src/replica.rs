use crate::depmap::DependencyMap;
use crate::errors::Result;
use crate::operation::{Operation, Operations};
use crate::storage::{Storage, TaskMap};
use crate::task::{Status, Tag, Task};
use crate::taskdb::TaskDb;
use crate::treemap::TreeMap;
use crate::{Error, TaskData};
use chrono::{DateTime, Duration, Utc};
use log::trace;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// A replica represents an instance of a user's task data, providing an easy interface
/// for querying and modifying that data.
///
/// ## Tasks
///
/// Tasks are uniquely identified by UUIDs. Most task modifications are performed via the
/// [`TaskData`](crate::TaskData) or [`Task`](crate::Task) types. The first is a lower-level type
/// that wraps the key-value store representing a task, while the second is a higher-level type
/// that supports methods to update specific properties, maintain dependencies and tags, and so on.
///
/// ## Operations
///
/// Changes to a replica occur by committing [`Operations`]s. All methods that change a replica
/// take an argument of type `&mut Operations`, and the necessary operations are added to that
/// sequence. Those changes may be reflected locally, such as in a [`Task`] or [`TaskData`] value, but
/// are not reflected in the Replica's storage until committed with [`Replica::commit_operations`].
/**
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
**/
/// Undo is supported by producing an [`Operations`] value representing the operations to be
/// undone. These are then committed with [`Replica::commit_reversed_operations`].
///
/// ## Working Set
///
/// A replica maintains a "working set" of tasks that are of current concern to the user,
/// specifically pending tasks.  These are indexed with small, easy-to-type integers.  Newly
/// pending tasks are automatically added to the working set, and the working set can be
/// "renumbered" when necessary.
pub struct Replica<S: Storage> {
    taskdb: TaskDb<S>,

    /// If true, this replica has already added an undo point.
    added_undo_point: bool,

    /// The dependency map for this replica, if it has been calculated.
    depmap: Option<Arc<DependencyMap>>,
}

impl<S: Storage> Replica<S> {
    pub fn new(storage: S) -> Replica<S> {
        Replica {
            taskdb: TaskDb::new(storage),
            added_undo_point: false,
            depmap: None,
        }
    }

    /// Update an existing task.  If the value is Some, the property is added or updated.  If the
    /// value is None, the property is deleted.  It is not an error to delete a nonexistent
    /// property.
    #[deprecated(since = "0.7.0", note = "please use TaskData instead")]
    pub async fn update_task<S1, S2>(
        &mut self,
        uuid: Uuid,
        property: S1,
        value: Option<S2>,
    ) -> Result<TaskMap>
    where
        S1: Into<String>,
        S2: Into<String>,
    {
        let value = value.map(|v| v.into());
        let property = property.into();
        let mut ops = self.make_operations();
        let Some(mut task) = self.get_task_data(uuid).await? else {
            return Err(Error::TaskNotFound(uuid));
        };
        task.update(property, value, &mut ops);
        self.commit_operations(ops).await?;
        Ok(self
            .taskdb
            .get_task(uuid)
            .await?
            .expect("task should exist after an update"))
    }

    /// Get all tasks represented as a map keyed by UUID
    pub async fn all_tasks(&mut self) -> Result<HashMap<Uuid, Task>> {
        let depmap = self.dependency_map(false).await?;
        let mut res = HashMap::new();
        for (uuid, tm) in self.taskdb.all_tasks().await?.drain(..) {
            res.insert(uuid, Task::new(TaskData::new(uuid, tm), depmap.clone()));
        }
        Ok(res)
    }

    /// Get all task represented as a map of [`TaskData`] keyed by UUID
    pub async fn all_task_data(&mut self) -> Result<HashMap<Uuid, TaskData>> {
        let mut res = HashMap::new();
        for (uuid, tm) in self.taskdb.all_tasks().await?.drain(..) {
            res.insert(uuid, TaskData::new(uuid, tm));
        }
        Ok(res)
    }

    /// Get the UUIDs of all tasks
    pub async fn all_task_uuids(&mut self) -> Result<Vec<Uuid>> {
        self.taskdb.all_task_uuids().await
    }

    /// Get an array containing all pending tasks
    pub async fn pending_tasks(&mut self) -> Result<Vec<Task>> {
        let depmap = self.dependency_map(false).await?;
        let res = self
            .pending_task_data()
            .await?
            .into_iter()
            .map(|taskdata| Task::new(taskdata, depmap.clone()))
            .collect();

        Ok(res)
    }

    pub async fn pending_task_data(&mut self) -> Result<Vec<TaskData>> {
        let res = self
            .taskdb
            .get_pending_tasks()
            .await?
            .into_iter()
            .map(|(uuid, taskmap)| TaskData::new(uuid, taskmap))
            .collect::<Vec<_>>();

        Ok(res)
    }

    /// Get all unique tag names across all tasks, sorted alphabetically.
    pub async fn get_all_tags(&mut self) -> Result<Vec<String>> {
        self.taskdb.get_all_tags().await
    }

    /// Get the raw JSON value of the `tc_settings` singleton row.
    ///
    /// Returns `None` if the row does not exist yet (first-use default).
    pub async fn get_tc_config(&mut self) -> Result<Option<String>> {
        self.taskdb.get_tc_config().await
    }

    /// Set the raw JSON value of the `tc_settings` singleton row.
    pub async fn set_tc_config(&mut self, value: String) -> Result<()> {
        self.taskdb.set_tc_config(value).await
    }

    /// Get the parsed [`TcConfig`] from storage.
    ///
    /// Returns the default config if the row does not exist yet.
    /// Returns `Err` if the stored JSON is malformed.
    pub async fn get_tc_config_parsed(&mut self) -> Result<crate::storage::tc_config::TcConfig> {
        match self.taskdb.get_tc_config().await? {
            None => Ok(crate::storage::tc_config::TcConfig::default()),
            Some(json) => serde_json::from_str(&json).map_err(|e| {
                crate::Error::Database(format!("Failed to parse tc_config JSON: {e}"))
            }),
        }
    }

    /// Set the config from a parsed [`TcConfig`] value.
    pub async fn set_tc_config_parsed(
        &mut self,
        config: &crate::storage::tc_config::TcConfig,
    ) -> Result<()> {
        let json = serde_json::to_string(config)
            .map_err(|e| crate::Error::Database(format!("Failed to serialize tc_config: {e}")))?;
        self.taskdb.set_tc_config(json).await
    }

    /// Remove `name` from tc_config.tags AND strip the `tag_{name}` key from every task
    /// in a single committed operation group.
    ///
    /// Task operations are committed first (with an undo point), then the config is
    /// persisted. If the task commit fails the config is left untouched. The config
    /// update is not reversible via `undo` — undo only reverses the task-level strip.
    ///
    /// Returns the number of tasks that had the tag removed.
    /// Returns `Err` if the tag is not present in tc_config.
    pub async fn delete_tag(&mut self, name: &str) -> Result<u32> {
        // Load and validate config first — fail fast before touching tasks.
        let mut config = self.get_tc_config_parsed().await?;
        if !config.remove_tag(name) {
            return Err(Error::Usage(format!("Tag not found: {name}")));
        }

        // Build task-strip operations using the actual stored values for `old_value`
        // so that undo correctly restores the tag presence.
        let tag_key = format!("tag_{name}");
        let all = self.taskdb.all_tasks().await?;
        let mut ops = Operations::new();
        ops.push(Operation::UndoPoint);
        let mut count = 0u32;
        for (uuid, taskmap) in &all {
            if taskmap.contains_key(&tag_key) {
                ops.push(Operation::Update {
                    uuid: *uuid,
                    property: tag_key.clone(),
                    old_value: taskmap.get(&tag_key).cloned(),
                    value: None,
                    timestamp: chrono::Utc::now(),
                });
                count += 1;
            }
        }

        // Commit task ops first; only persist config if that succeeds.
        self.commit_operations(ops).await?;
        self.set_tc_config_parsed(&config).await?;
        Ok(count)
    }

    /// Rename `old` → `new` in tc_config.tags AND rename `tag_{old}` → `tag_{new}` on every
    /// task in a single committed operation group.
    ///
    /// Task operations are committed first (with an undo point), then the config is
    /// persisted. If the task commit fails the config is left untouched. The config
    /// update is not reversible via `undo`.
    ///
    /// Returns the number of tasks that had the tag renamed.
    pub async fn rename_tag(&mut self, old: &str, new: &str) -> Result<u32> {
        // Validate new tag name.
        let _: Tag = new
            .try_into()
            .map_err(|e| Error::Usage(format!("Invalid tag name: {e}")))?;

        // Load and validate config first — fail fast before touching tasks.
        let mut config = self.get_tc_config_parsed().await?;
        config.rename_tag(old, new).map_err(Error::Usage)?;

        // Build task-rename operations using actual stored values for `old_value`.
        let old_key = format!("tag_{old}");
        let new_key = format!("tag_{new}");
        let all = self.taskdb.all_tasks().await?;
        let mut ops = Operations::new();
        ops.push(Operation::UndoPoint);
        let mut count = 0u32;
        for (uuid, taskmap) in &all {
            if taskmap.contains_key(&old_key) {
                ops.push(Operation::Update {
                    uuid: *uuid,
                    property: old_key.clone(),
                    old_value: taskmap.get(&old_key).cloned(),
                    value: None,
                    timestamp: chrono::Utc::now(),
                });
                ops.push(Operation::Update {
                    uuid: *uuid,
                    property: new_key.clone(),
                    old_value: None,
                    value: Some(String::new()),
                    timestamp: chrono::Utc::now(),
                });
                count += 1;
            }
        }

        // Commit task ops first; only persist config if that succeeds.
        self.commit_operations(ops).await?;
        self.set_tc_config_parsed(&config).await?;
        Ok(count)
    }

    /// Add `tag` to `task`, first validating that the tag is registered in tc_config.
    ///
    /// Returns `Err(Error::Usage(...))` if the tag is not in `tc_config.tags`.
    /// The tag is not added to the task in that case.
    ///
    /// This is the preferred entry point for external callers (e.g. cxx bridge).
    /// The UniFFI path uses batch pre-validation in `mutate_task` for efficiency.
    pub async fn add_tag_validated(
        &mut self,
        task: &mut Task,
        tag: &Tag,
        ops: &mut Operations,
    ) -> Result<()> {
        let config = self.get_tc_config_parsed().await?;
        if !config.has_tag(tag.as_ref()) {
            return Err(Error::Usage(format!("Tag not found in config: {tag}")));
        }
        task.add_tag(tag, ops)
    }

    /// Remove `name` from tc_config.xstatus AND clear the `xstatus` UDA key from every task
    /// whose xstatus value matches `name`, in a single committed operation group.
    ///
    /// Task operations are committed first (with an undo point), then the config is
    /// persisted. If the task commit fails the config is left untouched. The config
    /// update is not reversible via `undo` — undo only reverses the task-level clear.
    ///
    /// Returns the number of tasks that had the xstatus cleared.
    /// Returns `Err` if the xstatus is not present in tc_config.
    pub async fn delete_xstatus(&mut self, name: &str) -> Result<u32> {
        // Load and validate config first — fail fast before touching tasks.
        let mut config = self.get_tc_config_parsed().await?;
        if !config.remove_xstatus(name) {
            return Err(Error::Usage(format!("XStatus not found: {name}")));
        }

        // Build task-clear operations: remove xstatus UDA from tasks matching this name.
        let all = self.taskdb.all_tasks().await?;
        let mut ops = Operations::new();
        ops.push(Operation::UndoPoint);
        let mut count = 0u32;
        for (uuid, taskmap) in &all {
            if taskmap.get("xstatus").map(|v| v.as_str()) == Some(name) {
                ops.push(Operation::Update {
                    uuid: *uuid,
                    property: "xstatus".to_string(),
                    old_value: taskmap.get("xstatus").cloned(),
                    value: None,
                    timestamp: chrono::Utc::now(),
                });
                count += 1;
            }
        }

        // Commit task ops first; only persist config if that succeeds.
        self.commit_operations(ops).await?;
        self.set_tc_config_parsed(&config).await?;
        Ok(count)
    }

    /// Rename `old` → `new` in tc_config.xstatus AND rename the `xstatus` UDA value on every
    /// task whose xstatus matches `old`, in a single committed operation group.
    ///
    /// Task operations are committed first (with an undo point), then the config is
    /// persisted. If the task commit fails the config is left untouched. The config
    /// update is not reversible via `undo`.
    ///
    /// Returns the number of tasks that had the xstatus renamed.
    pub async fn rename_xstatus(&mut self, old: &str, new: &str) -> Result<u32> {
        // Load and validate config first — fail fast before touching tasks.
        let mut config = self.get_tc_config_parsed().await?;
        config.rename_xstatus(old, new).map_err(Error::Usage)?;

        // Build task-rename operations: update xstatus UDA value from old → new.
        let all = self.taskdb.all_tasks().await?;
        let mut ops = Operations::new();
        ops.push(Operation::UndoPoint);
        let mut count = 0u32;
        for (uuid, taskmap) in &all {
            if taskmap.get("xstatus").map(|v| v.as_str()) == Some(old) {
                ops.push(Operation::Update {
                    uuid: *uuid,
                    property: "xstatus".to_string(),
                    old_value: taskmap.get("xstatus").cloned(),
                    value: Some(new.to_string()),
                    timestamp: chrono::Utc::now(),
                });
                count += 1;
            }
        }

        // Commit task ops first; only persist config if that succeeds.
        self.commit_operations(ops).await?;
        self.set_tc_config_parsed(&config).await?;
        Ok(count)
    }

    /// Get the dependency map for all pending tasks.
    ///
    /// A task dependency is recognized when a task in the working set depends on a task with
    /// status equal to Pending.
    ///
    /// The data in this map is cached when it is first requested and may not contain modifications
    /// made locally in this Replica instance.  The result is reference-counted and may
    /// outlive the Replica.
    ///
    /// If `force` is true, then the result is re-calculated from the current state of the replica,
    /// although previously-returned dependency maps are not updated.
    ///
    /// Calculating this value requires a scan of the full working set and may not be performant.
    /// The [`TaskData`] API avoids generating this value.
    pub async fn dependency_map(&mut self, force: bool) -> Result<Arc<DependencyMap>> {
        if force || self.depmap.is_none() {
            // note: we can't use self.get_task here, as that depends on a
            // DependencyMap

            let mut dm = DependencyMap::new();
            // temporary cache tracking whether tasks are considered Pending or not.
            let mut is_pending_cache: HashMap<Uuid, bool> = HashMap::new();
            let pending = self.taskdb.get_pending_tasks().await?;
            for (u, taskmap) in &pending {
                // search the task's keys
                for p in taskmap.keys() {
                    // for one matching `dep_..`
                    if let Some(dep_str) = p.strip_prefix("dep_") {
                        // and extract the UUID from the key
                        if let Ok(dep) = Uuid::parse_str(dep_str) {
                            // the dependency is pending if
                            let dep_pending = {
                                // we've determined this before and cached the result
                                if let Some(dep_pending) = is_pending_cache.get(&dep) {
                                    *dep_pending
                                } else if let Some(dep_taskmap) =
                                    // or if we get the task
                                    self.taskdb.get_task(dep).await?
                                {
                                    // and its status is "pending"
                                    let dep_pending = matches!(
                                        dep_taskmap
                                            .get("status")
                                            .map(|tm| Status::from_taskmap(tm)),
                                        Some(Status::Pending)
                                    );
                                    is_pending_cache.insert(dep, dep_pending);
                                    dep_pending
                                } else {
                                    false
                                }
                            };
                            if dep_pending {
                                dm.add_dependency(*u, dep);
                            }
                        }
                    }
                }
            }
            self.depmap = Some(Arc::new(dm));
        }

        // at this point self.depmap is guaranteed to be Some(_)
        Ok(self.depmap.as_ref().unwrap().clone())
    }

    /// Get the tree map for all tasks.
    ///
    /// The tree map represents parent/child relationships between tasks using the `parent`
    /// property.  Unlike [`Replica::dependency_map`], this scans *all* tasks (not just the
    /// working set), so it includes completed and deleted tasks as well.
    ///
    /// The result is not cached — it is rebuilt on every call.  For typical task counts
    /// this is fast enough; caching can be added later if profiling shows a need.
    pub async fn tree_map(&mut self) -> Result<Arc<TreeMap>> {
        let tasks = self.all_tasks().await?;
        Ok(Arc::new(TreeMap::from_tasks(&tasks)))
    }

    /// Get an existing task by its UUID
    pub async fn get_task(&mut self, uuid: Uuid) -> Result<Option<Task>> {
        let depmap = self.dependency_map(false).await?;
        Ok(self
            .taskdb
            .get_task(uuid)
            .await?
            .map(move |tm| Task::new(TaskData::new(uuid, tm), depmap)))
    }

    /// Get an existing task by its UUID, as a [`TaskData`](crate::TaskData).
    pub async fn get_task_data(&mut self, uuid: Uuid) -> Result<Option<TaskData>> {
        Ok(self
            .taskdb
            .get_task(uuid)
            .await?
            .map(move |tm| TaskData::new(uuid, tm)))
    }

    /// Get the operations that led to the given task.
    ///
    /// This set of operations is suitable for providing an overview of the task history, but does
    /// not satisfy any invariants around operations and task state. That is, it is not guaranteed
    /// that the returned operations, if applied in order, would generate the current task state.
    ///
    /// It is also not guaranteed to be the same on every replica. Differences can occur when
    /// conflicting operations were performed on different replicas. The "losing" operations in
    /// those conflicts may not appear on all replicas. In practice, conflicts are rare and the
    /// results of this function will be the same on all replicas for most tasks.
    pub async fn get_task_operations(&mut self, uuid: Uuid) -> Result<Operations> {
        self.taskdb.get_task_operations(uuid).await
    }

    /// Create a new task, setting `modified`, `description`, `status`, and `entry`.
    ///
    /// This uses the high-level task interface. To create a task with the low-level
    /// interface, use [`TaskData::create`](crate::TaskData::create).
    #[deprecated(
        since = "0.7.0",
        note = "please use `create_task` and call `Task` methods `set_status`, `set_description`, and `set_entry`"
    )]
    pub async fn new_task(&mut self, status: Status, description: String) -> Result<Task> {
        let uuid = Uuid::new_v4();
        let mut ops = self.make_operations();
        let now = format!("{}", Utc::now().timestamp());
        let mut task = TaskData::create(uuid, &mut ops);
        task.update("modified", Some(now.clone()), &mut ops);
        task.update("description", Some(description), &mut ops);
        task.update("status", Some(status.to_taskmap().to_string()), &mut ops);
        task.update("entry", Some(now), &mut ops);
        self.commit_operations(ops).await?;
        trace!("task {uuid} created");
        Ok(self
            .get_task(uuid)
            .await?
            .expect("Task should exist after creation"))
    }

    /// Create a new task.
    ///
    /// Use [Uuid::new_v4] to invent a new task ID, if necessary. If the task already
    /// exists, it is returned.
    pub async fn create_task(&mut self, uuid: Uuid, ops: &mut Operations) -> Result<Task> {
        if let Some(task) = self.get_task(uuid).await? {
            return Ok(task);
        }
        let depmap = self.dependency_map(false).await?;
        Ok(Task::new(TaskData::create(uuid, ops), depmap))
    }

    /// Delete a task.  The task must exist.  Note that this is different from setting status to
    /// Deleted; this is the final purge of the task.
    ///
    /// Deletion may interact poorly with modifications to the same task on other replicas. For
    /// example, if a task is deleted on replica 1 and its description modified on replica 1, then
    /// after both replicas have fully synced, the resulting task will only have a `description`
    /// property.
    #[deprecated(since = "0.7.0", note = "please use TaskData::delete")]
    pub async fn delete_task(&mut self, uuid: Uuid) -> Result<()> {
        let Some(mut task) = self.get_task_data(uuid).await? else {
            return Err(Error::TaskNotFound(uuid));
        };
        let mut ops = self.make_operations();
        task.delete(&mut ops);
        self.commit_operations(ops).await?;
        trace!("task {uuid} deleted");
        Ok(())
    }

    /// Commit a set of operations to the replica.
    ///
    /// All local state on the replica will be updated accordingly, including temporarily cached
    /// data.
    pub async fn commit_operations(&mut self, operations: Operations) -> Result<()> {
        if operations.is_empty() {
            return Ok(());
        }

        self.taskdb.commit_operations(operations).await?;

        // The cached dependency map may now be invalid, do not retain it. Any existing Task values
        // will continue to use the old map.
        self.depmap = None;

        Ok(())
    }

    /// Return the operations back to and including the last undo point, or since the last sync if
    /// no undo point is found.
    ///
    /// The operations are returned in the order they were applied. Use
    /// [`Replica::commit_reversed_operations`] to "undo" them.
    pub async fn get_undo_operations(&mut self) -> Result<Operations> {
        self.taskdb.get_undo_operations().await
    }

    /// Get the number of operations local to this replica, excluding undo points.
    pub async fn num_local_operations(&mut self) -> Result<usize> {
        self.taskdb.num_operations().await
    }

    /// Get the number of undo points available (number of times `undo` will succeed).
    pub async fn num_undo_points(&mut self) -> Result<usize> {
        self.taskdb.num_undo_points().await
    }

    /// Commit the reverse of the given operations, beginning with the last operation in the given
    /// operations and proceeding to the first.
    ///
    /// This method only supports reversing operations if they precisely match local operations
    /// that have not yet been synchronized, and will return `false` if this is not the case.
    pub async fn commit_reversed_operations(&mut self, operations: Operations) -> Result<bool> {
        if !self.taskdb.commit_reversed_operations(operations).await? {
            return Ok(false);
        }

        // The dependency map is potentially now invalid.
        self.depmap = None;

        Ok(true)
    }

    /// Expire old, deleted tasks.
    ///
    /// Expiration entails removal of tasks from the replica. Any modifications that occur after
    /// the deletion (such as operations synchronized from other replicas) will do nothing.
    ///
    /// Tasks are eligible for expiration when they have status Deleted and have not been modified
    /// for 180 days (about six months). Note that completed tasks are not eligible.
    pub async fn expire_tasks(&mut self) -> Result<()> {
        let six_mos_ago = Utc::now() - Duration::days(180);
        let mut ops = Operations::new();
        let deleted = Status::Deleted.to_taskmap();
        self.all_task_data()
            .await?
            .drain()
            .filter(|(_, t)| t.get("status") == Some(deleted))
            .filter(|(_, t)| {
                t.get("modified").is_some_and(|m| {
                    m.parse().is_ok_and(|time_sec| {
                        DateTime::from_timestamp(time_sec, 0).is_some_and(|dt| dt < six_mos_ago)
                    })
                })
            })
            .for_each(|(_, mut t)| t.delete(&mut ops));
        self.commit_operations(ops).await
    }
    /// Add an UndoPoint, if one has not already been added by this Replica.  This occurs
    /// automatically when a change is made.  The `force` flag allows forcing a new UndoPoint
    /// even if one has already been created by this Replica, and may be useful when a Replica
    /// instance is held for a long time and used to apply more than one user-visible change.
    #[deprecated(
        since = "0.7.0",
        note = "Push an `Operation::UndoPoint` onto your `Operations` instead."
    )]
    pub async fn add_undo_point(&mut self, force: bool) -> Result<()> {
        if force || !self.added_undo_point {
            let ops = vec![Operation::UndoPoint];
            self.commit_operations(ops).await?;
            self.added_undo_point = true;
        }
        Ok(())
    }

    /// Make a new `Operations`, with an undo operation if one has not already been added by
    /// this `Replica` insance
    fn make_operations(&mut self) -> Operations {
        let mut ops = Operations::new();
        if !self.added_undo_point {
            ops.push(Operation::UndoPoint);
            self.added_undo_point = true;
        }
        ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{storage::inmemory::InMemoryStorage, task::Status};
    use chrono::{DateTime, TimeZone};
    use pretty_assertions::assert_eq;
    use std::collections::HashSet;
    use uuid::Uuid;

    const JUST_NOW: Option<DateTime<Utc>> = DateTime::from_timestamp(1800000000, 0);

    /// Rewrite automatically-created dates to "just-now" or `JUST_NOW` for ease of testing.
    fn clean_op(op: Operation) -> Operation {
        if let Operation::Update {
            uuid,
            property,
            mut old_value,
            mut value,
            ..
        } = op
        {
            if property == "modified" || property == "end" || property == "entry" {
                if value.is_some() {
                    value = Some("just-now".into());
                }
                if old_value.is_some() {
                    old_value = Some("just-now".into());
                }
            }
            Operation::Update {
                uuid,
                property,
                old_value,
                value,
                timestamp: JUST_NOW.unwrap(),
            }
        } else {
            op
        }
    }

    #[tokio::test]
    async fn new_task() {
        let mut rep = Replica::new(InMemoryStorage::new());

        #[allow(deprecated)]
        let t = rep
            .new_task(Status::Pending, "a task".into())
            .await
            .unwrap();
        assert_eq!(t.get_description(), String::from("a task"));
        assert_eq!(t.get_status(), Status::Pending);
        assert!(t.get_modified().is_some());
    }

    #[tokio::test]
    async fn modify_task() {
        let mut rep = Replica::new(InMemoryStorage::new());

        // Further test the deprecated `new_task` method.
        #[allow(deprecated)]
        let mut t = rep
            .new_task(Status::Pending, "a task".into())
            .await
            .unwrap();

        let mut ops = Operations::new();
        t.set_description(String::from("past tense"), &mut ops)
            .unwrap();
        t.set_status(Status::Completed, &mut ops).unwrap();
        // check that values have changed on the Task
        assert_eq!(t.get_description(), "past tense");
        assert_eq!(t.get_status(), Status::Completed);

        // check that values have not changed in storage, yet
        let t = rep.get_task(t.get_uuid()).await.unwrap().unwrap();
        assert_eq!(t.get_description(), "a task");
        assert_eq!(t.get_status(), Status::Pending);

        // check that values have changed in storage after commit
        rep.commit_operations(ops).await.unwrap();
        let t = rep.get_task(t.get_uuid()).await.unwrap().unwrap();
        assert_eq!(t.get_description(), "past tense");
        assert_eq!(t.get_status(), Status::Completed);

        // and check for the corresponding operations, cleaning out the timestamps
        // and modified properties as these are based on the current time
        assert_eq!(
            rep.taskdb
                .operations()
                .await
                .into_iter()
                .map(clean_op)
                .collect::<Vec<_>>(),
            vec![
                Operation::UndoPoint,
                Operation::Create { uuid: t.get_uuid() },
                Operation::Update {
                    uuid: t.get_uuid(),
                    property: "modified".into(),
                    old_value: None,
                    value: Some("just-now".into()),
                    timestamp: JUST_NOW.unwrap(),
                },
                Operation::Update {
                    uuid: t.get_uuid(),
                    property: "description".into(),
                    old_value: None,
                    value: Some("a task".into()),
                    timestamp: JUST_NOW.unwrap(),
                },
                Operation::Update {
                    uuid: t.get_uuid(),
                    property: "status".into(),
                    old_value: None,
                    value: Some("pending".into()),
                    timestamp: JUST_NOW.unwrap(),
                },
                Operation::Update {
                    uuid: t.get_uuid(),
                    property: "entry".into(),
                    old_value: None,
                    value: Some("just-now".into()),
                    timestamp: JUST_NOW.unwrap(),
                },
                Operation::Update {
                    uuid: t.get_uuid(),
                    property: "modified".into(),
                    old_value: Some("just-now".into()),
                    value: Some("just-now".into()),
                    timestamp: JUST_NOW.unwrap(),
                },
                Operation::Update {
                    uuid: t.get_uuid(),
                    property: "description".into(),
                    old_value: Some("a task".into()),
                    value: Some("past tense".into()),
                    timestamp: JUST_NOW.unwrap(),
                },
                Operation::Update {
                    uuid: t.get_uuid(),
                    property: "end".into(),
                    old_value: None,
                    value: Some("just-now".into()),
                    timestamp: JUST_NOW.unwrap(),
                },
                Operation::Update {
                    uuid: t.get_uuid(),
                    property: "status".into(),
                    old_value: Some("pending".into()),
                    value: Some("completed".into()),
                    timestamp: JUST_NOW.unwrap(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn delete_task() {
        let mut rep = Replica::new(InMemoryStorage::new());

        let uuid = Uuid::new_v4();
        let mut ops = Operations::new();
        rep.create_task(uuid, &mut ops).await.unwrap();
        rep.commit_operations(ops).await.unwrap();

        #[allow(deprecated)]
        rep.delete_task(uuid).await.unwrap();
        assert_eq!(rep.get_task(uuid).await.unwrap(), None);
    }

    #[tokio::test]
    async fn all_tasks() {
        let mut rep = Replica::new(InMemoryStorage::new());

        let (uuid1, uuid2) = (Uuid::new_v4(), Uuid::new_v4());
        let mut ops = Operations::new();
        rep.create_task(uuid1, &mut ops).await.unwrap();
        rep.create_task(uuid2, &mut ops).await.unwrap();
        rep.commit_operations(ops).await.unwrap();

        let all_tasks = rep.all_tasks().await.unwrap();
        assert_eq!(all_tasks.len(), 2);
        assert_eq!(all_tasks.get(&uuid1).unwrap().get_uuid(), uuid1);
        assert_eq!(all_tasks.get(&uuid2).unwrap().get_uuid(), uuid2);

        let all_tasks = rep.all_task_data().await.unwrap();
        assert_eq!(all_tasks.len(), 2);
        assert_eq!(all_tasks.get(&uuid1).unwrap().get_uuid(), uuid1);
        assert_eq!(all_tasks.get(&uuid2).unwrap().get_uuid(), uuid2);

        let mut all_uuids = rep.all_task_uuids().await.unwrap();
        all_uuids.sort();
        let mut exp_uuids = vec![uuid1, uuid2];
        exp_uuids.sort();
        assert_eq!(all_uuids.len(), 2);
        assert_eq!(all_uuids, exp_uuids);
    }

    #[tokio::test]
    async fn pending_tasks() {
        let mut rep = Replica::new(InMemoryStorage::new());

        let (uuid1, uuid2, uuid3) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mut ops = Operations::new();

        let mut t1 = rep.create_task(uuid1, &mut ops).await.unwrap();
        t1.set_status(Status::Pending, &mut ops).unwrap();

        let mut t2 = rep.create_task(uuid2, &mut ops).await.unwrap();
        t2.set_status(Status::Pending, &mut ops).unwrap();

        let mut t3 = rep.create_task(uuid3, &mut ops).await.unwrap();
        t3.set_status(Status::Completed, &mut ops).unwrap();

        rep.commit_operations(ops).await.unwrap();

        let mut pending_tasks = rep.pending_tasks().await.unwrap();
        pending_tasks.sort_by_key(|t| t.get_uuid());
        assert_eq!(pending_tasks.len(), 2);
        let mut expected = [uuid1, uuid2];
        expected.sort();
        assert_eq!(pending_tasks[0].get_uuid(), expected[0]);
        assert_eq!(pending_tasks[1].get_uuid(), expected[1]);
    }

    #[tokio::test]
    async fn commit_operations() -> Result<()> {
        let mut rep = Replica::new(InMemoryStorage::new());

        // Generate the depmap so later assertions can verify it is reset.
        rep.dependency_map(true).await.unwrap();
        assert!(rep.depmap.is_some());

        let mut ops = Operations::new();
        let uuid1 = Uuid::new_v4();
        let mut t = rep.create_task(uuid1, &mut ops).await.unwrap();
        t.set_status(Status::Pending, &mut ops).unwrap();
        rep.commit_operations(ops).await?;

        // Cached dependency map was reset.
        assert!(rep.depmap.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn commit_reversed_operations() -> Result<()> {
        let uuid1 = Uuid::new_v4();
        let uuid2 = Uuid::new_v4();
        let uuid3 = Uuid::new_v4();

        let mut rep = Replica::new(InMemoryStorage::new());

        let mut ops = Operations::new();
        ops.push(Operation::UndoPoint);
        rep.create_task(uuid1, &mut ops).await.unwrap();
        ops.push(Operation::UndoPoint);
        rep.create_task(uuid2, &mut ops).await.unwrap();
        rep.commit_operations(ops).await?;

        // Trying to reverse-commit the wrong operations fails.
        let ops = vec![Operation::Delete {
            uuid: uuid3,
            old_task: TaskMap::new(),
        }];
        assert!(!rep.commit_reversed_operations(ops).await?);

        // Commiting the correct operations succeeds
        let ops = rep.get_undo_operations().await?;
        assert_eq!(rep.num_undo_points().await.unwrap(), 2);
        assert!(rep.commit_reversed_operations(ops).await?);
        assert_eq!(rep.num_undo_points().await.unwrap(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn num_local_operations_and_undo_points() -> Result<()> {
        let mut rep = Replica::new(InMemoryStorage::new());

        let mut ops = Operations::new();
        ops.push(Operation::UndoPoint);
        let uuid1 = Uuid::new_v4();
        rep.create_task(uuid1, &mut ops).await.unwrap();
        let uuid2 = Uuid::new_v4();
        rep.create_task(uuid2, &mut ops).await.unwrap();
        rep.commit_operations(ops).await?;

        // 2 Create ops, 1 UndoPoint
        assert_eq!(rep.num_local_operations().await?, 2);
        assert_eq!(rep.num_undo_points().await?, 1);

        // A second undo point is counted.
        let ops = vec![Operation::UndoPoint];
        rep.commit_operations(ops).await?;
        assert_eq!(rep.num_undo_points().await?, 2);

        Ok(())
    }

    #[tokio::test]
    async fn get_and_modify() {
        let mut rep = Replica::new(InMemoryStorage::new());

        let mut ops = Operations::new();
        let uuid = Uuid::new_v4();
        let mut t = rep.create_task(uuid, &mut ops).await.unwrap();
        t.set_status(Status::Pending, &mut ops).unwrap();
        t.set_description("another task".into(), &mut ops).unwrap();
        rep.commit_operations(ops).await.unwrap();

        let mut t = rep.get_task(uuid).await.unwrap().unwrap();
        assert_eq!(t.get_description(), String::from("another task"));

        let mut ops = Operations::new();
        t.set_status(Status::Deleted, &mut ops).unwrap();
        t.set_description("gone".into(), &mut ops).unwrap();
        rep.commit_operations(ops).await.unwrap();

        let t = rep.get_task(uuid).await.unwrap().unwrap();
        assert_eq!(t.get_status(), Status::Deleted);
        assert_eq!(t.get_description(), "gone");
    }

    #[tokio::test]
    async fn get_task_data_and_operations() {
        let mut rep = Replica::new(InMemoryStorage::new());

        let uuid1 = Uuid::new_v4();
        let uuid2 = Uuid::new_v4();
        let mut ops = Operations::new();
        let mut t = rep.create_task(uuid1, &mut ops).await.unwrap();
        t.set_description("another task".into(), &mut ops).unwrap();
        let mut t2 = rep.create_task(uuid2, &mut ops).await.unwrap();
        t2.set_description("a distraction!".into(), &mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();

        let t = rep.get_task_data(uuid1).await.unwrap().unwrap();
        assert_eq!(t.get_uuid(), uuid1);
        assert_eq!(t.get("description"), Some("another task"));
        assert_eq!(
            rep.get_task_operations(uuid1)
                .await
                .unwrap()
                .into_iter()
                .map(clean_op)
                .collect::<Vec<_>>(),
            vec![
                Operation::Create { uuid: uuid1 },
                Operation::Update {
                    uuid: uuid1,
                    property: "modified".into(),
                    old_value: None,
                    value: Some("just-now".into()),
                    timestamp: JUST_NOW.unwrap(),
                },
                Operation::Update {
                    uuid: uuid1,
                    property: "description".into(),
                    old_value: None,
                    value: Some("another task".into()),
                    timestamp: JUST_NOW.unwrap(),
                },
            ]
        );

        assert!(rep.get_task_data(Uuid::new_v4()).await.unwrap().is_none());
        assert_eq!(
            rep.get_task_operations(Uuid::new_v4()).await.unwrap(),
            vec![]
        );
    }

    #[tokio::test]
    async fn get_does_not_exist() {
        let mut rep = Replica::new(InMemoryStorage::new());
        let uuid = Uuid::new_v4();
        assert_eq!(rep.get_task(uuid).await.unwrap(), None);
    }

    #[tokio::test]
    async fn expire() {
        let mut rep = Replica::new(InMemoryStorage::new());
        let mut ops = Operations::new();

        // uuid1 is old but pending, so is not expired.
        let keeper_uuid1 = Uuid::new_v4();
        let mut t = rep.create_task(keeper_uuid1, &mut ops).await.unwrap();
        t.set_description("keeper 1".into(), &mut ops).unwrap();
        t.set_modified(Utc.with_ymd_and_hms(1980, 1, 1, 0, 0, 0).unwrap(), &mut ops)
            .unwrap();
        t.set_status(Status::Pending, &mut ops).unwrap();

        // uuid2 is old but completed, so is not expired.
        let keeper_uuid2 = Uuid::new_v4();
        let mut t = rep.create_task(keeper_uuid2, &mut ops).await.unwrap();
        t.set_description("keeper 2".into(), &mut ops).unwrap();
        t.set_modified(Utc.with_ymd_and_hms(1980, 1, 1, 0, 0, 0).unwrap(), &mut ops)
            .unwrap();
        t.set_status(Status::Completed, &mut ops).unwrap();

        // uuid3 is deleted but recently modified, so is not expired.
        let keeper_uuid3 = Uuid::new_v4();
        let mut t = rep.create_task(keeper_uuid3, &mut ops).await.unwrap();
        t.set_description("keeper 3".into(), &mut ops).unwrap();
        t.set_status(Status::Deleted, &mut ops).unwrap();
        t.set_modified(Utc::now(), &mut ops).unwrap();
        t.set_entry(Some(Utc::now()), &mut ops).unwrap();

        // uuid4 was deleted long ago, so it is expired.
        let goner_uuid4 = Uuid::new_v4();
        let mut t = rep.create_task(goner_uuid4, &mut ops).await.unwrap();
        t.set_description("goner".into(), &mut ops).unwrap();
        t.set_status(Status::Deleted, &mut ops).unwrap();
        t.set_modified(Utc.with_ymd_and_hms(1980, 1, 1, 0, 0, 0).unwrap(), &mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();

        rep.expire_tasks().await.unwrap();

        assert!(rep.get_task_data(keeper_uuid1).await.unwrap().is_some());
        assert!(rep.get_task_data(keeper_uuid2).await.unwrap().is_some());
        assert!(rep.get_task_data(keeper_uuid3).await.unwrap().is_some());
        assert!(rep.get_task_data(goner_uuid4).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn dependency_map() {
        let mut rep = Replica::new(InMemoryStorage::new());

        let mut tasks = vec![];
        let mut ops = Operations::new();
        for _ in 0..4 {
            let mut t = rep.create_task(Uuid::new_v4(), &mut ops).await.unwrap();
            t.set_status(Status::Pending, &mut ops).unwrap();
            tasks.push(t);
        }
        let uuids: Vec<_> = tasks.iter().map(|t| t.get_uuid()).collect();

        // t[3] depends on t[2], and t[1]
        let mut t = tasks.pop().unwrap();
        t.add_dependency(uuids[2], &mut ops).unwrap();
        t.add_dependency(uuids[1], &mut ops).unwrap();

        // t[2] depends on t[0]
        let mut t = tasks.pop().unwrap();
        t.add_dependency(uuids[0], &mut ops).unwrap();

        // t[1] depends on t[0]
        let mut t = tasks.pop().unwrap();
        t.add_dependency(uuids[0], &mut ops).unwrap();

        rep.commit_operations(ops).await.unwrap();

        // generate the dependency map, forcing an update based on the newly-added dependencies.
        // This need not be forced since the `commit_operations` invalidated the cached value.
        let dm = rep.dependency_map(false).await.unwrap();

        assert_eq!(
            dm.dependencies(uuids[3]).collect::<HashSet<_>>(),
            HashSet::from([uuids[1], uuids[2]])
        );
        assert_eq!(
            dm.dependencies(uuids[2]).collect::<HashSet<_>>(),
            HashSet::from([uuids[0]])
        );
        assert_eq!(
            dm.dependencies(uuids[1]).collect::<HashSet<_>>(),
            HashSet::from([uuids[0]])
        );
        assert_eq!(
            dm.dependencies(uuids[0]).collect::<HashSet<_>>(),
            HashSet::from([])
        );

        assert_eq!(
            dm.dependents(uuids[3]).collect::<HashSet<_>>(),
            HashSet::from([])
        );
        assert_eq!(
            dm.dependents(uuids[2]).collect::<HashSet<_>>(),
            HashSet::from([uuids[3]])
        );
        assert_eq!(
            dm.dependents(uuids[1]).collect::<HashSet<_>>(),
            HashSet::from([uuids[3]])
        );
        assert_eq!(
            dm.dependents(uuids[0]).collect::<HashSet<_>>(),
            HashSet::from([uuids[1], uuids[2]])
        );

        // mark t[0] as done, removing it from the working set
        let mut ops = Operations::new();
        rep.get_task(uuids[0])
            .await
            .unwrap()
            .unwrap()
            .done(&mut ops)
            .unwrap();
        rep.commit_operations(ops).await.unwrap();
        let dm = rep.dependency_map(false).await.unwrap();

        assert_eq!(
            dm.dependencies(uuids[3]).collect::<HashSet<_>>(),
            HashSet::from([uuids[1], uuids[2]])
        );
        assert_eq!(
            dm.dependencies(uuids[2]).collect::<HashSet<_>>(),
            HashSet::from([])
        );
        assert_eq!(
            dm.dependencies(uuids[1]).collect::<HashSet<_>>(),
            HashSet::from([])
        );
        assert_eq!(
            dm.dependents(uuids[0]).collect::<HashSet<_>>(),
            HashSet::from([])
        );
    }

    #[tokio::test]
    async fn tree_map() {
        let mut rep = Replica::new(InMemoryStorage::new());

        // Create a parent task and three children with positions
        let parent_uuid = Uuid::new_v4();
        let child1_uuid = Uuid::new_v4();
        let child2_uuid = Uuid::new_v4();
        let child3_uuid = Uuid::new_v4();

        let mut ops = Operations::new();
        let mut parent = rep.create_task(parent_uuid, &mut ops).await.unwrap();
        parent.set_status(Status::Pending, &mut ops).unwrap();

        let mut c1 = rep.create_task(child1_uuid, &mut ops).await.unwrap();
        c1.set_status(Status::Pending, &mut ops).unwrap();
        c1.set_parent(Some(parent_uuid), &mut ops).unwrap();
        c1.set_position(Some("80".into()), &mut ops).unwrap();

        let mut c2 = rep.create_task(child2_uuid, &mut ops).await.unwrap();
        c2.set_status(Status::Pending, &mut ops).unwrap();
        c2.set_parent(Some(parent_uuid), &mut ops).unwrap();
        c2.set_position(Some("V0".into()), &mut ops).unwrap();

        let mut c3 = rep.create_task(child3_uuid, &mut ops).await.unwrap();
        c3.set_status(Status::Completed, &mut ops).unwrap();
        c3.set_parent(Some(parent_uuid), &mut ops).unwrap();

        rep.commit_operations(ops).await.unwrap();

        let tm = rep.tree_map().await.unwrap();

        // Parent is a root
        assert!(tm.roots().contains(&parent_uuid));
        // Children are not roots
        assert!(!tm.roots().contains(&child1_uuid));
        assert!(!tm.roots().contains(&child2_uuid));

        // tree_map scans all tasks — completed child3 is included
        let children = tm.children(parent_uuid);
        assert_eq!(children.len(), 3);
        // Positioned children come first in lex order
        assert_eq!(children[0], child1_uuid); // "80"
        assert_eq!(children[1], child2_uuid); // "V0"

        // All three are descendants
        let desc = tm.descendants(parent_uuid);
        assert!(desc.contains(&child1_uuid));
        assert!(desc.contains(&child2_uuid));
        assert!(desc.contains(&child3_uuid));

        // Only pending children returned by pending_child_ids
        let pending = tm.pending_child_ids(parent_uuid);
        assert!(pending.contains(&child1_uuid));
        assert!(pending.contains(&child2_uuid));
        assert!(!pending.contains(&child3_uuid)); // completed
    }

    // ── tc_config helpers ──────────────────────────────────────────────────

    async fn make_replica_with_tag(tag: &str) -> (Replica<InMemoryStorage>, Uuid) {
        let mut replica = Replica::new(InMemoryStorage::new());
        // Set up tc_config with the tag.
        let mut config = crate::storage::tc_config::TcConfig::default();
        config.tags = tag.to_string();
        replica.set_tc_config_parsed(&config).await.unwrap();
        // Create a task with the tag.
        let uuid = Uuid::new_v4();
        let mut ops = Operations::new();
        replica.create_task(uuid, &mut ops).await.unwrap();
        ops.push(Operation::Update {
            uuid,
            property: format!("tag_{tag}"),
            old_value: None,
            value: Some(String::new()),
            timestamp: Utc::now(),
        });
        replica.commit_operations(ops).await.unwrap();
        (replica, uuid)
    }

    #[tokio::test]
    async fn delete_tag_updates_config_and_task() {
        let (mut replica, task_uuid) = make_replica_with_tag("work").await;
        let count = replica.delete_tag("work").await.unwrap();
        assert_eq!(count, 1, "one task should have the tag removed");
        // Config no longer has 'work'.
        let config = replica.get_tc_config_parsed().await.unwrap();
        assert!(!config.has_tag("work"));
        // Task no longer has the tag key.
        let task = replica.get_task(task_uuid).await.unwrap().unwrap();
        assert!(task.get_value("tag_work").is_none());
    }

    #[tokio::test]
    async fn delete_tag_nonexistent_returns_err() {
        let mut replica = Replica::new(InMemoryStorage::new());
        let result = replica.delete_tag("ghost").await;
        assert!(result.is_err(), "expected error for nonexistent tag");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Tag not found"), "unexpected: {msg}");
    }

    #[tokio::test]
    async fn delete_tag_multi_tag_isolation() {
        // Task has both 'work' and 'home'. Deleting 'work' leaves 'home'.
        let mut replica = Replica::new(InMemoryStorage::new());
        let mut config = crate::storage::tc_config::TcConfig::default();
        config.tags = "work,home".to_string();
        replica.set_tc_config_parsed(&config).await.unwrap();

        let uuid = Uuid::new_v4();
        let mut ops = Operations::new();
        replica.create_task(uuid, &mut ops).await.unwrap();
        ops.push(Operation::Update {
            uuid,
            property: "tag_work".to_string(),
            old_value: None,
            value: Some(String::new()),
            timestamp: Utc::now(),
        });
        ops.push(Operation::Update {
            uuid,
            property: "tag_home".to_string(),
            old_value: None,
            value: Some(String::new()),
            timestamp: Utc::now(),
        });
        replica.commit_operations(ops).await.unwrap();

        let count = replica.delete_tag("work").await.unwrap();
        assert_eq!(count, 1);

        let task = replica.get_task(uuid).await.unwrap().unwrap();
        assert!(
            task.get_value("tag_work").is_none(),
            "tag_work should be removed"
        );
        assert!(
            task.get_value("tag_home").is_some(),
            "tag_home should remain"
        );
    }

    #[tokio::test]
    async fn rename_tag_success() {
        let (mut replica, task_uuid) = make_replica_with_tag("oldtag").await;
        let count = replica.rename_tag("oldtag", "newtag").await.unwrap();
        assert_eq!(count, 1);
        // Config updated.
        let config = replica.get_tc_config_parsed().await.unwrap();
        assert!(!config.has_tag("oldtag"));
        assert!(config.has_tag("newtag"));
        // Task key renamed.
        let task = replica.get_task(task_uuid).await.unwrap().unwrap();
        assert!(task.get_value("tag_oldtag").is_none());
        assert!(task.get_value("tag_newtag").is_some());
    }

    #[tokio::test]
    async fn rename_tag_invalid_name() {
        let (mut replica, _) = make_replica_with_tag("work").await;
        let result = replica.rename_tag("work", "INVALID TAG!").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rename_tag_nonexistent_old() {
        let mut replica = Replica::new(InMemoryStorage::new());
        let result = replica.rename_tag("ghost", "other").await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Tag not found"), "unexpected: {msg}");
    }

    #[tokio::test]
    async fn rename_tag_duplicate_new() {
        // Both 'old' and 'new' exist in config — rename 'old' → 'new' should fail.
        let mut replica = Replica::new(InMemoryStorage::new());
        let mut config = crate::storage::tc_config::TcConfig::default();
        config.tags = "old,new".to_string();
        replica.set_tc_config_parsed(&config).await.unwrap();
        let result = replica.rename_tag("old", "new").await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Tag already exists"), "unexpected: {msg}");
    }

    // ── add_tag_validated ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn add_tag_validated_returns_error_for_unregistered_tag() {
        let mut replica = Replica::new(InMemoryStorage::new());
        // No tags registered in config — adding any tag should fail.
        let task_uuid = Uuid::new_v4();
        let mut ops = Operations::new();
        let mut task = replica.create_task(task_uuid, &mut ops).await.unwrap();
        replica.commit_operations(ops).await.unwrap();

        let tag: Tag = "work".try_into().unwrap();
        let mut ops2 = Operations::new();
        let err = replica
            .add_tag_validated(&mut task, &tag, &mut ops2)
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Usage(_)),
            "expected Error::Usage, got: {err:?}"
        );
        // No partial operations should have been appended before the error.
        assert!(
            ops2.is_empty(),
            "ops must be unmodified after validation failure"
        );
    }

    #[tokio::test]
    async fn add_tag_validated_succeeds_for_registered_tag() {
        let mut replica = Replica::new(InMemoryStorage::new());
        // Register 'work' in config.
        let mut config = crate::storage::tc_config::TcConfig::default();
        config.add_tag("work");
        replica.set_tc_config_parsed(&config).await.unwrap();

        let task_uuid = Uuid::new_v4();
        let mut ops = Operations::new();
        let mut task = replica.create_task(task_uuid, &mut ops).await.unwrap();
        replica.commit_operations(ops).await.unwrap();

        let tag: Tag = "work".try_into().unwrap();
        let mut ops2 = Operations::new();
        ops2.push(Operation::UndoPoint);
        replica
            .add_tag_validated(&mut task, &tag, &mut ops2)
            .await
            .unwrap();
        replica.commit_operations(ops2).await.unwrap();

        let updated = replica.get_task(task_uuid).await.unwrap().unwrap();
        assert!(
            updated.get_value("tag_work").is_some(),
            "tag_work should be set on the task"
        );
    }

    // ── xstatus lifecycle helpers ─────────────────────────────────────────

    async fn make_replica_with_xstatus(name: &str) -> (Replica<InMemoryStorage>, Uuid) {
        use crate::storage::tc_config::XStatusDef;

        let mut replica = Replica::new(InMemoryStorage::new());
        // Set up tc_config with the xstatus definition.
        let mut config = crate::storage::tc_config::TcConfig::default();
        config.add_xstatus(XStatusDef {
            name: name.to_string(),
            icon: 128721,
        });
        replica.set_tc_config_parsed(&config).await.unwrap();
        // Create a task with the xstatus UDA value.
        let uuid = Uuid::new_v4();
        let mut ops = Operations::new();
        replica.create_task(uuid, &mut ops).await.unwrap();
        ops.push(Operation::Update {
            uuid,
            property: "xstatus".to_string(),
            old_value: None,
            value: Some(name.to_string()),
            timestamp: Utc::now(),
        });
        replica.commit_operations(ops).await.unwrap();
        (replica, uuid)
    }

    // ── delete_xstatus ────────────────────────────────────────────────────

    #[tokio::test]
    async fn delete_xstatus_updates_config_and_task() {
        let (mut replica, task_uuid) = make_replica_with_xstatus("blocked").await;
        let count = replica.delete_xstatus("blocked").await.unwrap();
        assert_eq!(count, 1, "one task should have the xstatus cleared");
        // Config no longer has 'blocked'.
        let config = replica.get_tc_config_parsed().await.unwrap();
        assert!(!config.has_xstatus("blocked"));
        // Task no longer has the xstatus key.
        let task = replica.get_task(task_uuid).await.unwrap().unwrap();
        assert!(task.get_value("xstatus").is_none());
    }

    #[tokio::test]
    async fn delete_xstatus_nonexistent_returns_err() {
        let mut replica = Replica::new(InMemoryStorage::new());
        let err = replica.delete_xstatus("ghost").await.unwrap_err();
        assert!(
            matches!(err, Error::Usage(_)),
            "expected Error::Usage, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn delete_xstatus_only_affects_matching_tasks() {
        use crate::storage::tc_config::XStatusDef;

        let mut replica = Replica::new(InMemoryStorage::new());
        let mut config = crate::storage::tc_config::TcConfig::default();
        config.add_xstatus(XStatusDef {
            name: "blocked".into(),
            icon: 1,
        });
        config.add_xstatus(XStatusDef {
            name: "waiting".into(),
            icon: 2,
        });
        replica.set_tc_config_parsed(&config).await.unwrap();

        // Task A with xstatus=blocked, task B with xstatus=waiting.
        let uuid_a = Uuid::new_v4();
        let uuid_b = Uuid::new_v4();
        let mut ops = Operations::new();
        replica.create_task(uuid_a, &mut ops).await.unwrap();
        ops.push(Operation::Update {
            uuid: uuid_a,
            property: "xstatus".to_string(),
            old_value: None,
            value: Some("blocked".to_string()),
            timestamp: Utc::now(),
        });
        replica.create_task(uuid_b, &mut ops).await.unwrap();
        ops.push(Operation::Update {
            uuid: uuid_b,
            property: "xstatus".to_string(),
            old_value: None,
            value: Some("waiting".to_string()),
            timestamp: Utc::now(),
        });
        replica.commit_operations(ops).await.unwrap();

        let count = replica.delete_xstatus("blocked").await.unwrap();
        assert_eq!(count, 1);
        // Task A cleared, task B untouched.
        let task_a = replica.get_task(uuid_a).await.unwrap().unwrap();
        assert!(task_a.get_value("xstatus").is_none());
        let task_b = replica.get_task(uuid_b).await.unwrap().unwrap();
        assert_eq!(task_b.get_value("xstatus").unwrap(), "waiting");
    }

    // ── rename_xstatus ────────────────────────────────────────────────────

    #[tokio::test]
    async fn rename_xstatus_success() {
        let (mut replica, task_uuid) = make_replica_with_xstatus("blocked").await;
        let count = replica.rename_xstatus("blocked", "waiting").await.unwrap();
        assert_eq!(count, 1, "one task should have the xstatus renamed");
        // Config updated.
        let config = replica.get_tc_config_parsed().await.unwrap();
        assert!(!config.has_xstatus("blocked"));
        assert!(config.has_xstatus("waiting"));
        // Task updated.
        let task = replica.get_task(task_uuid).await.unwrap().unwrap();
        assert_eq!(task.get_value("xstatus").unwrap(), "waiting");
    }

    #[tokio::test]
    async fn rename_xstatus_nonexistent_old() {
        let mut replica = Replica::new(InMemoryStorage::new());
        let err = replica.rename_xstatus("ghost", "new").await.unwrap_err();
        assert!(
            matches!(err, Error::Usage(_)),
            "expected Error::Usage, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn rename_xstatus_duplicate_new() {
        use crate::storage::tc_config::XStatusDef;

        let mut replica = Replica::new(InMemoryStorage::new());
        let mut config = crate::storage::tc_config::TcConfig::default();
        config.add_xstatus(XStatusDef {
            name: "old".into(),
            icon: 1,
        });
        config.add_xstatus(XStatusDef {
            name: "new".into(),
            icon: 2,
        });
        replica.set_tc_config_parsed(&config).await.unwrap();

        let err = replica.rename_xstatus("old", "new").await.unwrap_err();
        assert!(
            matches!(err, Error::Usage(_)),
            "expected Error::Usage, got: {err:?}"
        );
    }
}
