//! FFI session and task query methods.
//!
//! [`FfiSession`] (Swift: `TCSession`) holds the executor and user identity.
//! All task operations are async methods on the session — UniFFI's `RustFuture`
//! polling mechanism drives execution from the foreign side, no tokio runtime
//! is needed.

use std::sync::Arc;
use taskchampion::{
    position::{append_position, between_position, prepend_position},
    storage::tc_config::TcConfig,
    ExternalStorage, Operation, Operations, Replica, Status,
};
use uuid::Uuid;

use chrono::Utc;

use crate::convert::{tree_map_to_ffi, FfiSqlExecutorAdapter};
use crate::types::{
    FfiDependencyEdge, FfiError, FfiSqlExecutor, FfiTask, FfiTreeNode, ReparentPosition,
};

// ---------------------------------------------------------------------------
// TCSession (FfiSession)
// ---------------------------------------------------------------------------

/// Holds the executor for a TaskChampion session.
///
/// Construct once at startup; all task operations are async methods
/// on this object. Each method creates an ephemeral [`Replica`] — no
/// persistent state is held between calls, making concurrent use safe.
///
/// Named `FfiSession` (not `TCSession`) due to UniFFI derive macro
/// limitations — see `types.rs` module docs.
#[derive(uniffi::Object)]
pub struct FfiSession {
    executor: Arc<dyn FfiSqlExecutor>,
}

#[uniffi::export]
impl FfiSession {
    /// Create a new session.
    #[uniffi::constructor]
    pub fn new(executor: Arc<dyn FfiSqlExecutor>) -> Arc<Self> {
        Arc::new(Self { executor })
    }
}

impl FfiSession {
    /// Build an ephemeral [`Replica`] from this session's executor and run `f` on it.
    pub(crate) async fn with_replica<F, Fut, T>(&self, f: F) -> Result<T, FfiError>
    where
        F: FnOnce(Replica<ExternalStorage>) -> Fut,
        Fut: std::future::Future<Output = Result<T, FfiError>>,
    {
        let adapter = FfiSqlExecutorAdapter::new(Arc::clone(&self.executor));
        let storage = ExternalStorage::new(Box::new(adapter));
        let replica = Replica::new(storage);
        f(replica).await
    }
}

// ---------------------------------------------------------------------------
// Exported async methods on FfiSession
// ---------------------------------------------------------------------------

#[uniffi::export]
impl FfiSession {
    /// Fetch a single task by UUID. Returns `None` if not found.
    pub async fn get_task(&self, uuid: String) -> Result<Option<FfiTask>, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid = parse_uuid(&uuid)?;
            let task = replica.get_task(uuid).await.map_err(FfiError::from)?;
            Ok(task.as_ref().map(FfiTask::from))
        })
        .await
    }

    /// Return all tasks (pending, completed, deleted).
    pub async fn all_tasks(&self) -> Result<Vec<FfiTask>, FfiError> {
        self.with_replica(|mut replica| async move {
            let tasks = replica.all_tasks().await.map_err(FfiError::from)?;
            Ok(tasks.values().map(FfiTask::from).collect())
        })
        .await
    }

    /// Return pending tasks only.
    pub async fn pending_tasks(&self) -> Result<Vec<FfiTask>, FfiError> {
        self.with_replica(|mut replica| async move {
            let tasks = replica.pending_tasks().await.map_err(FfiError::from)?;
            Ok(tasks.iter().map(FfiTask::from).collect())
        })
        .await
    }

    /// Return the task tree as a flat list of [`FfiTreeNode`]s.
    pub async fn tree_map(&self) -> Result<Vec<FfiTreeNode>, FfiError> {
        self.with_replica(|mut replica| async move {
            let tm = replica.tree_map().await.map_err(FfiError::from)?;
            Ok(tree_map_to_ffi(&tm))
        })
        .await
    }

    /// Return all dependency edges as `(from_uuid depends_on to_uuid)` pairs.
    pub async fn dependency_map(&self) -> Result<Vec<FfiDependencyEdge>, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuids = replica.all_task_uuids().await.map_err(FfiError::from)?;
            let dm = replica
                .dependency_map(false)
                .await
                .map_err(FfiError::from)?;
            let mut edges = Vec::new();
            for uuid in &uuids {
                for dep in dm.dependencies(*uuid) {
                    edges.push(FfiDependencyEdge {
                        from_uuid: uuid.to_string(),
                        to_uuid: dep.to_string(),
                    });
                }
            }
            Ok(edges)
        })
        .await
    }

    /// Create a new task with the given UUID and description.
    ///
    /// The task is immediately committed with `status: Pending` and `entry: now`.
    pub async fn create_task(
        &self,
        uuid: String,
        description: String,
    ) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            let task_uuid = parse_uuid(&uuid)?;
            // Reject duplicate creates upfront — replica.create_task silently
            // returns the existing task, so we must guard here to surface the
            // structured error to Swift callers.
            //
            // TOCTOU note: this is a best-effort check. Under PowerSync's
            // serialized single-writer model concurrent races are not possible,
            // so the check→create window is safe in practice.
            if replica
                .get_task(task_uuid)
                .await
                .map_err(FfiError::from)?
                .is_some()
            {
                return Err(FfiError::TaskAlreadyExists { uuid: uuid.clone() });
            }
            let mut ops = Operations::new();
            ops.push(Operation::UndoPoint);
            let mut task = replica
                .create_task(task_uuid, &mut ops)
                .await
                .map_err(FfiError::from)?;
            task.set_description(description, &mut ops)
                .map_err(FfiError::from)?;
            task.set_status(Status::Pending, &mut ops)
                .map_err(FfiError::from)?;
            task.set_entry(Some(Utc::now()), &mut ops)
                .map_err(FfiError::from)?;
            replica
                .commit_operations(ops)
                .await
                .map_err(FfiError::from)?;
            // Re-fetch to get the dependency-map-aware Task
            let created = replica
                .get_task(task_uuid)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::Internal {
                    message: "Task missing after create".into(),
                })?;
            Ok(FfiTask::from(&created))
        })
        .await
    }

    /// Atomically undo the last operation group.
    ///
    /// Returns `true` if an undo was performed, `false` if there is nothing to undo.
    pub async fn undo(&self) -> Result<bool, FfiError> {
        self.with_replica(|mut replica| async move {
            let ops = replica
                .get_undo_operations()
                .await
                .map_err(FfiError::from)?;
            if ops.is_empty() {
                return Ok(false);
            }
            replica
                .commit_reversed_operations(ops)
                .await
                .map_err(FfiError::from)
        })
        .await
    }

    /// Remove `name` from tc_config.tags and strip `tag_{name}` from all tasks atomically.
    ///
    /// Returns the number of tasks that had the tag removed.
    /// Returns `TagNotFound` if the tag is not in tc_config.
    pub async fn delete_tag(&self, name: String) -> Result<u32, FfiError> {
        self.with_replica(|mut replica| async move {
            let mut ops = Operations::new();
            ops.push(Operation::UndoPoint);
            let count = replica
                .delete_tag(&name, &mut ops)
                .await
                .map_err(|e| match e {
                    taskchampion::Error::Usage(ref msg) if msg.starts_with("Tag not found") => {
                        FfiError::TagNotFound { name: name.clone() }
                    }
                    other => FfiError::from(other),
                })?;
            replica
                .commit_operations(ops)
                .await
                .map_err(FfiError::from)?;
            Ok(count)
        })
        .await
    }

    /// Rename `old` to `new` in tc_config.tags and across all task keys atomically.
    ///
    /// Returns the number of tasks updated.
    /// Returns `TagNotFound` if `old` is not in tc_config.
    /// Returns `TagAlreadyExists` if `new` is already in tc_config.
    /// Returns `InvalidInput` if `new` is not a valid tag name.
    pub async fn rename_tag(&self, old: String, new: String) -> Result<u32, FfiError> {
        self.with_replica(|mut replica| async move {
            let mut ops = Operations::new();
            ops.push(Operation::UndoPoint);
            let count = replica
                .rename_tag(&old, &new, &mut ops)
                .await
                .map_err(|e| match e {
                    taskchampion::Error::Usage(ref msg) if msg.starts_with("tag not found") => {
                        FfiError::TagNotFound { name: old.clone() }
                    }
                    taskchampion::Error::Usage(ref msg)
                        if msg.starts_with("tag already exists") =>
                    {
                        FfiError::TagAlreadyExists { name: new.clone() }
                    }
                    taskchampion::Error::Usage(ref msg) if msg.starts_with("Invalid tag name") => {
                        FfiError::InvalidInput {
                            message: msg.clone(),
                        }
                    }
                    other => FfiError::from(other),
                })?;
            replica
                .commit_operations(ops)
                .await
                .map_err(FfiError::from)?;
            Ok(count)
        })
        .await
    }
}

/// Load tc_config from replica, returning a default if absent.
async fn load_tc_config(replica: &mut Replica<ExternalStorage>) -> Result<TcConfig, FfiError> {
    replica.get_tc_config_parsed().await.map_err(FfiError::from)
}

// ---------------------------------------------------------------------------
// xstatus methods
// ---------------------------------------------------------------------------

#[uniffi::export]
impl FfiSession {
    /// Set the xstatus UDA on a task. Validates that `name` is in tc_config.xstatus.
    ///
    /// Also auto-sets status to `Pending` if the task is not already pending.
    /// Returns `UnknownXStatus` if `name` is not in tc_config.xstatus definitions.
    pub async fn set_xstatus(&self, task_uuid: String, name: String) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid = parse_uuid(&task_uuid)?;
            let config = load_tc_config(&mut replica).await?;
            if !config.has_xstatus(&name) {
                return Err(FfiError::UnknownXStatus { name });
            }

            let mut task = replica
                .get_task(uuid)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound {
                    uuid: task_uuid.clone(),
                })?;

            let mut ops = Operations::new();
            ops.push(Operation::UndoPoint);

            // Set xstatus UDA.
            task.set_value("xstatus", Some(name), &mut ops)
                .map_err(FfiError::from)?;

            // Auto-set status to pending if not already pending.
            if task.get_status() != taskchampion::Status::Pending {
                task.set_status(Status::Pending, &mut ops)
                    .map_err(FfiError::from)?;
            }

            replica
                .commit_operations(ops)
                .await
                .map_err(FfiError::from)?;

            let updated = replica
                .get_task(uuid)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::Internal {
                    message: "Task missing after set_xstatus".into(),
                })?;
            Ok(FfiTask::from(&updated))
        })
        .await
    }

    /// Clear the xstatus UDA on a task, and auto-set status to `Pending`.
    pub async fn clear_xstatus(&self, task_uuid: String) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid = parse_uuid(&task_uuid)?;

            let mut task = replica
                .get_task(uuid)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound {
                    uuid: task_uuid.clone(),
                })?;

            let mut ops = Operations::new();
            ops.push(Operation::UndoPoint);

            // Clear xstatus UDA.
            task.set_value("xstatus", None::<String>, &mut ops)
                .map_err(FfiError::from)?;

            // Auto-set status to pending.
            if task.get_status() != taskchampion::Status::Pending {
                task.set_status(Status::Pending, &mut ops)
                    .map_err(FfiError::from)?;
            }

            replica
                .commit_operations(ops)
                .await
                .map_err(FfiError::from)?;

            let updated = replica
                .get_task(uuid)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::Internal {
                    message: "Task missing after clear_xstatus".into(),
                })?;
            Ok(FfiTask::from(&updated))
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Reorder methods
// ---------------------------------------------------------------------------

#[uniffi::export]
impl FfiSession {
    /// Move `uuid` to a position immediately after `anchor_uuid` among their shared siblings.
    ///
    /// Both tasks must have the same parent (or both be root tasks).
    /// Returns `TaskNotFound` if either UUID does not exist or has no position.
    /// Returns `NotASibling` if the two tasks have different parents.
    pub async fn reorder_after(
        &self,
        uuid: String,
        anchor_uuid: String,
    ) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid_parsed = parse_uuid(&uuid)?;
            let anchor_parsed = parse_uuid(&anchor_uuid)?;

            // Load both tasks to verify existence and parent.
            let task = replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound { uuid: uuid.clone() })?;
            let anchor_task = replica
                .get_task(anchor_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound {
                    uuid: anchor_uuid.clone(),
                })?;

            // Verify same parent.
            if task.get_parent() != anchor_task.get_parent() {
                return Err(FfiError::NotASibling {
                    uuid: uuid.clone(),
                    anchor: anchor_uuid.clone(),
                });
            }

            // Get siblings excluding uuid, sorted by position string.
            let tm = replica.tree_map().await.map_err(FfiError::from)?;
            let mut siblings = tm.sibling_positions(task.get_parent(), Some(uuid_parsed));
            siblings.sort_by(|(_, a), (_, b)| a.cmp(b));

            // Find anchor's index and position.
            let anchor_idx = siblings
                .iter()
                .position(|(u, _)| *u == anchor_parsed)
                .ok_or_else(|| FfiError::TaskNotFound {
                    uuid: anchor_uuid.clone(),
                })?;
            let anchor_pos = &siblings[anchor_idx].1;

            // Compute new position.
            let new_pos = if anchor_idx + 1 == siblings.len() {
                // Anchor is last sibling — append after it.
                append_position(Some(anchor_pos.as_str())).map_err(|e| FfiError::InvalidInput {
                    message: e.to_string(),
                })?
            } else {
                let next_pos = &siblings[anchor_idx + 1].1;
                between_position(anchor_pos.as_str(), next_pos.as_str()).map_err(|e| {
                    FfiError::InvalidInput {
                        message: e.to_string(),
                    }
                })?
            };

            // Apply the new position.
            let mut ops = Operations::new();
            ops.push(Operation::UndoPoint);
            let mut task_mut = replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::Internal {
                    message: "Task missing before set_position".into(),
                })?;
            task_mut
                .set_position(Some(new_pos), &mut ops)
                .map_err(FfiError::from)?;
            replica
                .commit_operations(ops)
                .await
                .map_err(FfiError::from)?;

            let updated = replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::Internal {
                    message: "Task missing after reorder_after".into(),
                })?;
            Ok(FfiTask::from(&updated))
        })
        .await
    }

    /// Move `uuid` to a position immediately before `anchor_uuid` among their shared siblings.
    ///
    /// Both tasks must have the same parent (or both be root tasks).
    /// Returns `TaskNotFound` if either UUID does not exist or has no position.
    /// Returns `NotASibling` if the two tasks have different parents.
    pub async fn reorder_before(
        &self,
        uuid: String,
        anchor_uuid: String,
    ) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid_parsed = parse_uuid(&uuid)?;
            let anchor_parsed = parse_uuid(&anchor_uuid)?;

            // Load both tasks to verify existence and parent.
            let task = replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound { uuid: uuid.clone() })?;
            let anchor_task = replica
                .get_task(anchor_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound {
                    uuid: anchor_uuid.clone(),
                })?;

            // Verify same parent.
            if task.get_parent() != anchor_task.get_parent() {
                return Err(FfiError::NotASibling {
                    uuid: uuid.clone(),
                    anchor: anchor_uuid.clone(),
                });
            }

            // Get siblings excluding uuid, sorted by position string.
            let tm = replica.tree_map().await.map_err(FfiError::from)?;
            let mut siblings = tm.sibling_positions(task.get_parent(), Some(uuid_parsed));
            siblings.sort_by(|(_, a), (_, b)| a.cmp(b));

            // Find anchor's index and position.
            let anchor_idx = siblings
                .iter()
                .position(|(u, _)| *u == anchor_parsed)
                .ok_or_else(|| FfiError::TaskNotFound {
                    uuid: anchor_uuid.clone(),
                })?;
            let anchor_pos = &siblings[anchor_idx].1;

            // Compute new position.
            let new_pos = if anchor_idx == 0 {
                // Anchor is first sibling — prepend before it.
                prepend_position(Some(anchor_pos.as_str())).map_err(|e| FfiError::InvalidInput {
                    message: e.to_string(),
                })?
            } else {
                let prev_pos = &siblings[anchor_idx - 1].1;
                between_position(prev_pos.as_str(), anchor_pos.as_str()).map_err(|e| {
                    FfiError::InvalidInput {
                        message: e.to_string(),
                    }
                })?
            };

            // Apply the new position.
            let mut ops = Operations::new();
            ops.push(Operation::UndoPoint);
            let mut task_mut = replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::Internal {
                    message: "Task missing before set_position".into(),
                })?;
            task_mut
                .set_position(Some(new_pos), &mut ops)
                .map_err(FfiError::from)?;
            replica
                .commit_operations(ops)
                .await
                .map_err(FfiError::from)?;

            let updated = replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::Internal {
                    message: "Task missing after reorder_before".into(),
                })?;
            Ok(FfiTask::from(&updated))
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Reparent and ancestor methods
// ---------------------------------------------------------------------------

#[uniffi::export]
impl FfiSession {
    /// Move `uuid` to a new parent with the specified position among siblings.
    ///
    /// Verifies that the move would not create a cycle (returns `CircularParent`
    /// if `new_parent` is a descendant of `uuid`).
    ///
    /// Returns `TaskNotFound` if `uuid` or `new_parent` do not exist.
    /// Returns `TaskNotFound` if an `After`/`Before` anchor does not exist
    /// under `new_parent`.
    /// Returns `CircularParent` if the move would create a cycle.
    pub async fn reparent(
        &self,
        uuid: String,
        new_parent: Option<String>,
        position: ReparentPosition,
    ) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid_parsed = parse_uuid_ctx(&uuid, "uuid")?;
            let new_parent_parsed: Option<Uuid> = new_parent
                .as_deref()
                .map(|s| parse_uuid_ctx(s, "new_parent"))
                .transpose()?;

            // Load uuid task to verify it exists.
            replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound { uuid: uuid.clone() })?;

            // Verify new_parent exists (if provided).
            if let Some(np_uuid) = new_parent_parsed {
                replica
                    .get_task(np_uuid)
                    .await
                    .map_err(FfiError::from)?
                    .ok_or_else(|| FfiError::TaskNotFound {
                        uuid: new_parent.clone().unwrap_or_default(),
                    })?;
            }

            // Cycle check: is new_parent a descendant of uuid?
            let tm = replica.tree_map().await.map_err(FfiError::from)?;
            if let Some(np_uuid) = new_parent_parsed {
                if tm.is_ancestor(np_uuid, uuid_parsed) {
                    return Err(FfiError::CircularParent {
                        uuid: uuid.clone(),
                        parent: new_parent.clone().unwrap_or_default(),
                    });
                }
            }

            // Compute new position under new_parent (sorted by position string for stable ordering).
            let mut siblings = tm.sibling_positions(new_parent_parsed, None);
            siblings.sort_by(|(_, a), (_, b)| a.cmp(b));
            let new_pos: Option<String> = match &position {
                ReparentPosition::End => {
                    let last_pos = siblings.last().map(|(_, p)| p.as_str());
                    Some(
                        append_position(last_pos).map_err(|e| FfiError::InvalidInput {
                            message: e.to_string(),
                        })?,
                    )
                }
                ReparentPosition::Beginning => {
                    let first_pos = siblings.first().map(|(_, p)| p.as_str());
                    Some(
                        prepend_position(first_pos).map_err(|e| FfiError::InvalidInput {
                            message: e.to_string(),
                        })?,
                    )
                }
                ReparentPosition::After { anchor } => {
                    let anchor_parsed = parse_uuid_ctx(anchor, "anchor")?;
                    let anchor_idx = siblings
                        .iter()
                        .position(|(u, _)| *u == anchor_parsed)
                        .ok_or_else(|| FfiError::TaskNotFound {
                            uuid: anchor.clone(),
                        })?;
                    let anchor_pos = &siblings[anchor_idx].1;
                    Some(if anchor_idx + 1 == siblings.len() {
                        append_position(Some(anchor_pos.as_str())).map_err(|e| {
                            FfiError::InvalidInput {
                                message: e.to_string(),
                            }
                        })?
                    } else {
                        let next_pos = &siblings[anchor_idx + 1].1;
                        between_position(anchor_pos.as_str(), next_pos.as_str()).map_err(|e| {
                            FfiError::InvalidInput {
                                message: e.to_string(),
                            }
                        })?
                    })
                }
                ReparentPosition::Before { anchor } => {
                    let anchor_parsed = parse_uuid_ctx(anchor, "anchor")?;
                    let anchor_idx = siblings
                        .iter()
                        .position(|(u, _)| *u == anchor_parsed)
                        .ok_or_else(|| FfiError::TaskNotFound {
                            uuid: anchor.clone(),
                        })?;
                    let anchor_pos = &siblings[anchor_idx].1;
                    Some(if anchor_idx == 0 {
                        prepend_position(Some(anchor_pos.as_str())).map_err(|e| {
                            FfiError::InvalidInput {
                                message: e.to_string(),
                            }
                        })?
                    } else {
                        let prev_pos = &siblings[anchor_idx - 1].1;
                        between_position(prev_pos.as_str(), anchor_pos.as_str()).map_err(|e| {
                            FfiError::InvalidInput {
                                message: e.to_string(),
                            }
                        })?
                    })
                }
            };

            // Apply parent + position atomically.
            let mut ops = Operations::new();
            ops.push(Operation::UndoPoint);
            let mut task_mut = replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::Internal {
                    message: "Task missing before reparent".into(),
                })?;
            task_mut
                .set_parent(new_parent_parsed, &mut ops)
                .map_err(FfiError::from)?;
            task_mut
                .set_position(new_pos, &mut ops)
                .map_err(FfiError::from)?;
            replica
                .commit_operations(ops)
                .await
                .map_err(FfiError::from)?;

            let updated = replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::Internal {
                    message: "Task missing after reparent".into(),
                })?;
            Ok(FfiTask::from(&updated))
        })
        .await
    }

    /// Return `true` if `ancestor_uuid` is an ancestor of `uuid` in the task tree.
    ///
    /// Used for UI hints such as greying out invalid drag-and-drop targets.
    /// The `reparent` method performs this check internally — callers do not need
    /// to call `is_ancestor` for safety.
    ///
    /// Returns `false` if either UUID does not exist or is not in the tree.
    pub async fn is_ancestor(&self, uuid: String, ancestor_uuid: String) -> Result<bool, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid_parsed = parse_uuid_ctx(&uuid, "uuid")?;
            let ancestor_parsed = parse_uuid_ctx(&ancestor_uuid, "ancestor_uuid")?;
            let tm = replica.tree_map().await.map_err(FfiError::from)?;
            Ok(tm.is_ancestor(uuid_parsed, ancestor_parsed))
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

pub(crate) fn parse_uuid(s: &str) -> Result<Uuid, FfiError> {
    Uuid::parse_str(s).map_err(|e| FfiError::InvalidInput {
        message: format!("Invalid UUID: {e}"),
    })
}

/// Parse a UUID string with a context label in the error message.
///
/// Prefer this over inlining `Uuid::parse_str(...).map_err(...)` at call sites.
/// The `ctx` label names the field (e.g. `"target"`, `"template UUID"`).
pub(crate) fn parse_uuid_ctx(s: &str, ctx: &str) -> Result<Uuid, FfiError> {
    Uuid::parse_str(s).map_err(|e| FfiError::InvalidInput {
        message: format!("invalid {ctx} '{s}': {e}"),
    })
}
