//! FFI session and task query methods.
//!
//! [`FfiSession`] (Swift: `TCSession`) holds the executor and user identity.
//! All task operations are async methods on the session — UniFFI's `RustFuture`
//! polling mechanism drives execution from the foreign side, no tokio runtime
//! is needed.

use std::sync::Arc;
use taskchampion::{ExternalStorage, Operation, Operations, Replica, Status};
use uuid::Uuid;

use chrono::Utc;

use crate::convert::{tree_map_to_ffi, FfiSqlExecutorAdapter};
use crate::types::{FfiDependencyEdge, FfiError, FfiSqlExecutor, FfiTask, FfiTreeNode};

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

    /// Get the color associated with a tag name.
    ///
    /// Returns an empty string if no color has been set for this tag.
    pub async fn get_tag_color(&self, name: String) -> Result<String, FfiError> {
        self.with_replica(|mut replica| async move {
            replica
                .get_tag_color(name)
                .await
                .map(|opt| opt.unwrap_or_default())
                .map_err(FfiError::from)
        })
        .await
    }

    /// Set the color for a tag name.
    ///
    /// If a color already exists for this tag, it is updated.
    pub async fn set_tag_color(&self, name: String, color: String) -> Result<(), FfiError> {
        self.with_replica(|mut replica| async move {
            replica
                .set_tag_color(name, color)
                .await
                .map_err(FfiError::from)
        })
        .await
    }

    /// Get all unique tag names across all tasks, sorted alphabetically.
    pub async fn get_all_tags(&self) -> Result<Vec<String>, FfiError> {
        self.with_replica(|mut replica| async move {
            replica.get_all_tags().await.map_err(FfiError::from)
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
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

pub(crate) fn parse_uuid(s: &str) -> Result<Uuid, FfiError> {
    Uuid::parse_str(s).map_err(|e| FfiError::InvalidInput {
        message: format!("Invalid UUID: {e}"),
    })
}
