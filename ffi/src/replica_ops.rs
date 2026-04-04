//! FFI session and task query methods.
//!
//! [`FfiSession`] (Swift: `TCSession`) holds the executor and user identity.
//! All task operations are async methods on the session — UniFFI's `RustFuture`
//! polling mechanism drives execution from the foreign side, no tokio runtime
//! is needed.

use std::collections::HashMap;
use std::sync::Arc;
use taskchampion::{
    position::{append_position, between_position, prepend_position},
    storage::tc_config::TcConfig,
    ExternalStorage, Operation, Operations, Replica, Status, Tag,
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

    /// Fetch a single task by UUID.
    ///
    /// Returns `None` if the task does not exist.
    pub async fn get_task(&self, uuid: String) -> Result<Option<FfiTask>, FfiError> {
        self.with_replica(|mut replica| async move {
            let task_uuid = parse_uuid(&uuid)?;
            let task = replica.get_task(task_uuid).await.map_err(FfiError::from)?;
            Ok(task.as_ref().map(FfiTask::from))
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

    /// Remove `name` from tc_config.tags and strip `tag_{name}` from all tasks.
    ///
    /// Task operations are committed first (undoable), then the config is persisted.
    /// Returns the number of tasks that had the tag removed.
    /// Returns `TagNotFound` if the tag is not in tc_config.
    pub async fn delete_tag(&self, name: String) -> Result<u32, FfiError> {
        self.with_replica(|mut replica| async move {
            replica.delete_tag(&name).await.map_err(|e| match e {
                taskchampion::Error::Usage(ref msg) if msg.starts_with("Tag not found") => {
                    FfiError::TagNotFound { name: name.clone() }
                }
                other => FfiError::from(other),
            })
        })
        .await
    }

    /// Rename `old` to `new` in tc_config.tags and across all task keys.
    ///
    /// Task operations are committed first (undoable), then the config is persisted.
    /// Returns the number of tasks updated.
    /// Returns `TagNotFound` if `old` is not in tc_config.
    /// Returns `TagAlreadyExists` if `new` is already in tc_config.
    /// Returns `InvalidInput` if `new` is not a valid tag name.
    pub async fn rename_tag(&self, old: String, new: String) -> Result<u32, FfiError> {
        self.with_replica(|mut replica| async move {
            replica.rename_tag(&old, &new).await.map_err(|e| match e {
                taskchampion::Error::Usage(ref msg) if msg.starts_with("Tag not found") => {
                    FfiError::TagNotFound { name: old.clone() }
                }
                taskchampion::Error::Usage(ref msg) if msg.starts_with("Tag already exists") => {
                    FfiError::TagAlreadyExists { name: new.clone() }
                }
                taskchampion::Error::Usage(ref msg) if msg.starts_with("Invalid tag name") => {
                    FfiError::InvalidInput {
                        message: msg.clone(),
                    }
                }
                other => FfiError::from(other),
            })
        })
        .await
    }

    /// Register `name` as a new tag in tc_config.
    ///
    /// Returns `TagAlreadyExists` if the tag is already in tc_config.
    /// Returns `InvalidInput` if `name` is not a valid tag name.
    ///
    /// Swift callers should call this before `mutate_task(AddTag)` to register
    /// new tags. Pairs with the CLI `task tag add` command on the tw side.
    pub async fn create_tag(&self, name: String) -> Result<(), FfiError> {
        self.with_replica(|mut replica| async move {
            // Validate tag name first — fail fast before reading config.
            // Also reject synthetic tags (e.g. "WAITING") — they are computed
            // at runtime and cannot be stored in tc_config.
            let tag: Tag = name
                .as_str()
                .try_into()
                .map_err(|e| FfiError::InvalidInput {
                    message: format!("Invalid tag name: {e}"),
                })?;
            if tag.is_synthetic() {
                return Err(FfiError::InvalidInput {
                    message: format!("'{name}' is a synthetic tag and cannot be registered"),
                });
            }

            let mut config = replica
                .get_tc_config_parsed()
                .await
                .map_err(FfiError::from)?;

            if !config.add_tag(&name) {
                return Err(FfiError::TagAlreadyExists { name });
            }

            replica
                .set_tc_config_parsed(&config)
                .await
                .map_err(FfiError::from)
        })
        .await
    }

    /// Register a new xstatus definition in tc_config.
    ///
    /// Returns `XStatusAlreadyExists` if the name is already registered.
    pub async fn create_xstatus(&self, name: String, icon: u32) -> Result<(), FfiError> {
        use taskchampion::storage::tc_config::XStatusDef;

        self.with_replica(|mut replica| async move {
            let mut config = replica
                .get_tc_config_parsed()
                .await
                .map_err(FfiError::from)?;

            if !config.add_xstatus(XStatusDef {
                name: name.clone(),
                icon,
            }) {
                return Err(FfiError::XStatusAlreadyExists { name });
            }

            replica
                .set_tc_config_parsed(&config)
                .await
                .map_err(FfiError::from)
        })
        .await
    }

    /// Remove an xstatus definition from tc_config and clear `xstatus` UDA from
    /// all tasks matching that name.
    ///
    /// Returns the number of tasks that had the xstatus cleared.
    /// Returns `XStatusNotFound` if the name is not in tc_config.xstatus.
    pub async fn delete_xstatus(&self, name: String) -> Result<u32, FfiError> {
        self.with_replica(|mut replica| async move {
            replica.delete_xstatus(&name).await.map_err(|e| match e {
                taskchampion::Error::Usage(ref msg) if msg.starts_with("XStatus not found") => {
                    FfiError::XStatusNotFound { name: name.clone() }
                }
                other => FfiError::from(other),
            })
        })
        .await
    }

    /// Rename an xstatus definition in tc_config and update the `xstatus` UDA value
    /// on all tasks matching the old name.
    ///
    /// Returns the number of tasks updated.
    /// Returns `XStatusNotFound` if `old` is not in tc_config.xstatus.
    /// Returns `XStatusAlreadyExists` if `new` is already in tc_config.xstatus.
    pub async fn rename_xstatus(&self, old: String, new: String) -> Result<u32, FfiError> {
        self.with_replica(|mut replica| async move {
            replica
                .rename_xstatus(&old, &new)
                .await
                .map_err(|e| match e {
                    taskchampion::Error::Usage(ref msg) if msg.starts_with("XStatus not found") => {
                        FfiError::XStatusNotFound { name: old.clone() }
                    }
                    taskchampion::Error::Usage(ref msg)
                        if msg.starts_with("XStatus already exists") =>
                    {
                        FfiError::XStatusAlreadyExists { name: new.clone() }
                    }
                    other => FfiError::from(other),
                })
        })
        .await
    }
}

/// Load tc_config from replica, returning a default if absent.
async fn load_tc_config(replica: &mut Replica<ExternalStorage>) -> Result<TcConfig, FfiError> {
    replica.get_tc_config_parsed().await.map_err(FfiError::from)
}

/// Set or clear the `xstatus` UDA on a task, committing atomically.
///
/// - `Some(name)`: sets xstatus and auto-restores `Pending` status if needed.
/// - `None`: clears xstatus and auto-restores `Pending` status if needed.
///   Returns the current task unchanged if xstatus is already `None` (no-op).
async fn write_xstatus(
    replica: &mut Replica<ExternalStorage>,
    uuid: uuid::Uuid,
    uuid_str: &str,
    value: Option<String>,
) -> Result<FfiTask, FfiError> {
    let mut task = replica
        .get_task(uuid)
        .await
        .map_err(FfiError::from)?
        .ok_or_else(|| FfiError::TaskNotFound {
            uuid: uuid_str.to_string(),
        })?;

    // Early return if clearing an already-None xstatus — avoid a vacuous undo point.
    if value.is_none() && task.get_value("xstatus").is_none() {
        return Ok(FfiTask::from(&task));
    }

    let mut ops = Operations::new();
    ops.push(Operation::UndoPoint);

    task.set_value("xstatus", value, &mut ops)
        .map_err(FfiError::from)?;

    // Auto-restore pending if the task is not already pending.
    if task.get_status() != taskchampion::Status::Pending {
        task.set_status(Status::Pending, &mut ops)
            .map_err(FfiError::from)?;
    }

    replica
        .commit_operations(ops)
        .await
        .map_err(FfiError::from)?;

    replica
        .get_task(uuid)
        .await
        .map_err(FfiError::from)?
        .ok_or_else(|| FfiError::Internal {
            message: "Task missing after write_xstatus".into(),
        })
        .map(|t| FfiTask::from(&t))
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
            write_xstatus(&mut replica, uuid, &task_uuid, Some(name)).await
        })
        .await
    }

    /// Clear the xstatus UDA on a task, and auto-set status to `Pending`.
    ///
    /// Returns the task unchanged (no undo point) if xstatus is already `None`.
    pub async fn clear_xstatus(&self, task_uuid: String) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid = parse_uuid(&task_uuid)?;
            write_xstatus(&mut replica, uuid, &task_uuid, None).await
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Position helpers (used by reorder_after, reorder_before, reparent)
// ---------------------------------------------------------------------------

/// Returns siblings of `parent` excluding `exclude`, sorted ascending by position string.
fn sorted_sibling_positions(
    tm: &taskchampion::TreeMap,
    parent: Option<uuid::Uuid>,
    exclude: Option<uuid::Uuid>,
) -> Vec<(uuid::Uuid, String)> {
    let mut siblings = tm.sibling_positions(parent, exclude);
    siblings.sort_by(|(_, a), (_, b)| a.cmp(b));
    siblings
}

/// Find the index of `anchor` in a sorted siblings slice.
///
/// Returns `AnchorHasNoPosition` when the anchor task exists in the DB but is
/// not in the positioned siblings list (i.e. it has no `position` field set).
fn find_anchor_idx(
    siblings: &[(uuid::Uuid, String)],
    anchor: uuid::Uuid,
    anchor_str: &str,
) -> Result<usize, FfiError> {
    siblings
        .iter()
        .position(|(u, _)| *u == anchor)
        .ok_or_else(|| FfiError::AnchorHasNoPosition {
            uuid: anchor_str.to_string(),
        })
}

/// Compute a position string immediately **after** `siblings[idx]`.
fn position_after_anchor(
    siblings: &[(uuid::Uuid, String)],
    idx: usize,
) -> Result<String, FfiError> {
    let anchor_pos = &siblings[idx].1;
    if idx + 1 == siblings.len() {
        append_position(Some(anchor_pos.as_str()))
    } else {
        between_position(anchor_pos.as_str(), &siblings[idx + 1].1)
    }
    .map_err(|e| FfiError::InvalidInput {
        message: e.to_string(),
    })
}

/// Compute a position string immediately **before** `siblings[idx]`.
fn position_before_anchor(
    siblings: &[(uuid::Uuid, String)],
    idx: usize,
) -> Result<String, FfiError> {
    let anchor_pos = &siblings[idx].1;
    if idx == 0 {
        prepend_position(Some(anchor_pos.as_str()))
    } else {
        between_position(&siblings[idx - 1].1, anchor_pos.as_str())
    }
    .map_err(|e| FfiError::InvalidInput {
        message: e.to_string(),
    })
}

/// Set `new_pos` on task `uuid`, commit with an undo point, and re-fetch.
async fn apply_position(
    replica: &mut Replica<ExternalStorage>,
    uuid: uuid::Uuid,
    new_pos: String,
) -> Result<FfiTask, FfiError> {
    let mut ops = Operations::new();
    ops.push(Operation::UndoPoint);
    let mut task = replica
        .get_task(uuid)
        .await
        .map_err(FfiError::from)?
        .ok_or_else(|| FfiError::Internal {
            message: "Task missing before set_position".into(),
        })?;
    task.set_position(Some(new_pos), &mut ops)
        .map_err(FfiError::from)?;
    replica
        .commit_operations(ops)
        .await
        .map_err(FfiError::from)?;
    replica
        .get_task(uuid)
        .await
        .map_err(FfiError::from)?
        .ok_or_else(|| FfiError::Internal {
            message: "Task missing after position change".into(),
        })
        .map(|t| FfiTask::from(&t))
}

// ---------------------------------------------------------------------------
// Reorder methods
// ---------------------------------------------------------------------------

#[uniffi::export]
impl FfiSession {
    /// Move `uuid` to a position immediately after `anchor_uuid` among their shared siblings.
    ///
    /// Both tasks must have the same parent (or both be root tasks).
    /// Returns `TaskNotFound` if either UUID does not exist in the database.
    /// Returns `AnchorHasNoPosition` if the anchor exists but has no position field.
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

            let tm = replica.tree_map().await.map_err(FfiError::from)?;
            let siblings = sorted_sibling_positions(&tm, task.get_parent(), Some(uuid_parsed));
            let idx = find_anchor_idx(&siblings, anchor_parsed, &anchor_uuid)?;
            let new_pos = position_after_anchor(&siblings, idx)?;
            apply_position(&mut replica, uuid_parsed, new_pos).await
        })
        .await
    }

    /// Move `uuid` to a position immediately before `anchor_uuid` among their shared siblings.
    ///
    /// Both tasks must have the same parent (or both be root tasks).
    /// Returns `TaskNotFound` if either UUID does not exist in the database.
    /// Returns `AnchorHasNoPosition` if the anchor exists but has no position field.
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

            let tm = replica.tree_map().await.map_err(FfiError::from)?;
            let siblings = sorted_sibling_positions(&tm, task.get_parent(), Some(uuid_parsed));
            let idx = find_anchor_idx(&siblings, anchor_parsed, &anchor_uuid)?;
            let new_pos = position_before_anchor(&siblings, idx)?;
            apply_position(&mut replica, uuid_parsed, new_pos).await
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Reorder to beginning/end methods
// ---------------------------------------------------------------------------

/// Shared implementation for `reorder_to_beginning` and `reorder_to_end`.
///
/// `pick_edge` selects the reference position from sorted siblings and computes
/// the new position string (prepend for beginning, append for end).
async fn reorder_to_edge<F>(
    replica: &mut Replica<ExternalStorage>,
    uuid_str: &str,
    pick_edge: F,
) -> Result<FfiTask, FfiError>
where
    F: FnOnce(&[(uuid::Uuid, String)]) -> Result<String, FfiError>,
{
    let uuid_parsed = parse_uuid(uuid_str)?;
    let task = replica
        .get_task(uuid_parsed)
        .await
        .map_err(FfiError::from)?
        .ok_or_else(|| FfiError::TaskNotFound {
            uuid: uuid_str.to_string(),
        })?;

    let tm = replica.tree_map().await.map_err(FfiError::from)?;
    let siblings = sorted_sibling_positions(&tm, task.get_parent(), Some(uuid_parsed));
    let new_pos = pick_edge(&siblings)?;
    apply_position(replica, uuid_parsed, new_pos).await
}

#[uniffi::export]
impl FfiSession {
    /// Move `uuid` to the first position among its current siblings.
    ///
    /// Returns `TaskNotFound` if the UUID does not exist.
    pub async fn reorder_to_beginning(&self, uuid: String) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            reorder_to_edge(&mut replica, &uuid, |siblings| {
                let first_pos = siblings.first().map(|(_, p)| p.as_str());
                prepend_position(first_pos).map_err(|e| FfiError::InvalidInput {
                    message: e.to_string(),
                })
            })
            .await
        })
        .await
    }

    /// Move `uuid` to the last position among its current siblings.
    ///
    /// Returns `TaskNotFound` if the UUID does not exist.
    pub async fn reorder_to_end(&self, uuid: String) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            reorder_to_edge(&mut replica, &uuid, |siblings| {
                let last_pos = siblings.last().map(|(_, p)| p.as_str());
                append_position(last_pos).map_err(|e| FfiError::InvalidInput {
                    message: e.to_string(),
                })
            })
            .await
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Today-view reorder helpers
// ---------------------------------------------------------------------------

/// Returns all tasks that have `today_position` set, excluding `exclude`,
/// sorted ascending by `today_position` string.
fn sorted_today_positions(
    all_tasks: &HashMap<uuid::Uuid, taskchampion::Task>,
    exclude: uuid::Uuid,
) -> Vec<(uuid::Uuid, String)> {
    let mut positioned: Vec<(uuid::Uuid, String)> = all_tasks
        .iter()
        .filter(|(&u, _)| u != exclude)
        .filter_map(|(&u, t)| t.get_value("today_position").map(|p| (u, p.to_string())))
        .collect();
    positioned.sort_by(|(_, a), (_, b)| a.cmp(b));
    positioned
}

/// Set `new_pos` as `today_position` on task `uuid`, commit with an undo point, and re-fetch.
async fn apply_today_position(
    replica: &mut Replica<ExternalStorage>,
    uuid: uuid::Uuid,
    new_pos: String,
) -> Result<FfiTask, FfiError> {
    let mut ops = Operations::new();
    ops.push(Operation::UndoPoint);
    let mut task = replica
        .get_task(uuid)
        .await
        .map_err(FfiError::from)?
        .ok_or_else(|| FfiError::Internal {
            message: "Task missing before set today_position".into(),
        })?;
    task.set_value("today_position", Some(new_pos), &mut ops)
        .map_err(FfiError::from)?;
    replica
        .commit_operations(ops)
        .await
        .map_err(FfiError::from)?;
    replica
        .get_task(uuid)
        .await
        .map_err(FfiError::from)?
        .ok_or_else(|| FfiError::Internal {
            message: "Task missing after set today_position".into(),
        })
        .map(|t| FfiTask::from(&t))
}

// ---------------------------------------------------------------------------
// Today-view reorder methods
// ---------------------------------------------------------------------------

#[uniffi::export]
impl FfiSession {
    /// Move `uuid` to a today_position immediately after `anchor_uuid` in the today view.
    ///
    /// Both tasks must exist. The anchor must have `today_position` set.
    /// Returns `TaskNotFound` if either UUID does not exist.
    /// Returns `AnchorHasNoPosition` if the anchor has no `today_position`.
    pub async fn today_reorder_after(
        &self,
        uuid: String,
        anchor_uuid: String,
    ) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid_parsed = parse_uuid(&uuid)?;
            let anchor_parsed = parse_uuid(&anchor_uuid)?;

            // Verify both tasks exist.
            replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound { uuid: uuid.clone() })?;
            replica
                .get_task(anchor_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound {
                    uuid: anchor_uuid.clone(),
                })?;

            let all = replica.all_tasks().await.map_err(FfiError::from)?;
            let today = sorted_today_positions(&all, uuid_parsed);
            let idx = find_anchor_idx(&today, anchor_parsed, &anchor_uuid)?;
            let new_pos = position_after_anchor(&today, idx)?;
            apply_today_position(&mut replica, uuid_parsed, new_pos).await
        })
        .await
    }

    /// Move `uuid` to a today_position immediately before `anchor_uuid` in the today view.
    ///
    /// Both tasks must exist. The anchor must have `today_position` set.
    /// Returns `TaskNotFound` if either UUID does not exist.
    /// Returns `AnchorHasNoPosition` if the anchor has no `today_position`.
    pub async fn today_reorder_before(
        &self,
        uuid: String,
        anchor_uuid: String,
    ) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid_parsed = parse_uuid(&uuid)?;
            let anchor_parsed = parse_uuid(&anchor_uuid)?;

            // Verify both tasks exist.
            replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound { uuid: uuid.clone() })?;
            replica
                .get_task(anchor_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound {
                    uuid: anchor_uuid.clone(),
                })?;

            let all = replica.all_tasks().await.map_err(FfiError::from)?;
            let today = sorted_today_positions(&all, uuid_parsed);
            let idx = find_anchor_idx(&today, anchor_parsed, &anchor_uuid)?;
            let new_pos = position_before_anchor(&today, idx)?;
            apply_today_position(&mut replica, uuid_parsed, new_pos).await
        })
        .await
    }

    /// Move `uuid` to the first position in the today view.
    ///
    /// Returns `TaskNotFound` if the UUID does not exist.
    pub async fn today_reorder_to_beginning(&self, uuid: String) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid_parsed = parse_uuid(&uuid)?;
            replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound { uuid: uuid.clone() })?;

            let all = replica.all_tasks().await.map_err(FfiError::from)?;
            let today = sorted_today_positions(&all, uuid_parsed);
            let first_pos = today.first().map(|(_, p)| p.as_str());
            let new_pos = prepend_position(first_pos).map_err(|e| FfiError::InvalidInput {
                message: e.to_string(),
            })?;
            apply_today_position(&mut replica, uuid_parsed, new_pos).await
        })
        .await
    }

    /// Move `uuid` to the last position in the today view.
    ///
    /// Returns `TaskNotFound` if the UUID does not exist.
    pub async fn today_reorder_to_end(&self, uuid: String) -> Result<FfiTask, FfiError> {
        self.with_replica(|mut replica| async move {
            let uuid_parsed = parse_uuid(&uuid)?;
            replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound { uuid: uuid.clone() })?;

            let all = replica.all_tasks().await.map_err(FfiError::from)?;
            let today = sorted_today_positions(&all, uuid_parsed);
            let last_pos = today.last().map(|(_, p)| p.as_str());
            let new_pos = append_position(last_pos).map_err(|e| FfiError::InvalidInput {
                message: e.to_string(),
            })?;
            apply_today_position(&mut replica, uuid_parsed, new_pos).await
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
    /// Returns `AnchorHasNoPosition` if an `After`/`Before` anchor exists but
    /// has no `position` field. Note: a malformed anchor UUID returns `InvalidInput`,
    /// not `TaskNotFound`.
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
                        uuid: np_uuid.to_string(),
                    })?;
            }

            // Cycle check: is new_parent a descendant of uuid?
            let tm = replica.tree_map().await.map_err(FfiError::from)?;
            if let Some(np_uuid) = new_parent_parsed {
                if tm.is_ancestor(np_uuid, uuid_parsed) {
                    return Err(FfiError::CircularParent {
                        uuid: uuid.clone(),
                        parent: np_uuid.to_string(),
                    });
                }
            }

            // Compute new position under new_parent (sorted by position string for stable ordering).
            let siblings = sorted_sibling_positions(&tm, new_parent_parsed, None);
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
                    let idx = find_anchor_idx(&siblings, anchor_parsed, anchor)?;
                    Some(position_after_anchor(&siblings, idx)?)
                }
                ReparentPosition::Before { anchor } => {
                    let anchor_parsed = parse_uuid_ctx(anchor, "anchor")?;
                    let idx = find_anchor_idx(&siblings, anchor_parsed, anchor)?;
                    Some(position_before_anchor(&siblings, idx)?)
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

            replica
                .get_task(uuid_parsed)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::Internal {
                    message: "Task missing after reparent".into(),
                })
                .map(|t| FfiTask::from(&t))
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
