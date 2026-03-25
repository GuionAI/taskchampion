//! Conversions between taskchampion types and FFI-friendly types.

use async_trait::async_trait;
use taskchampion::{Status, Task, TreeMap};
use uuid::Uuid;

use crate::types::{FfiAnnotation, FfiError, FfiStatus, FfiTask, FfiTreeNode};

// ---------------------------------------------------------------------------
// Task → FfiTask
// ---------------------------------------------------------------------------

impl From<&Task> for FfiTask {
    fn from(task: &Task) -> Self {
        FfiTask {
            uuid: task.get_uuid().to_string(),
            status: FfiStatus::from(task.get_status()),
            description: task.get_description().to_string(),
            priority: task.get_priority().to_string(),
            // Timestamp is pub(crate) in taskchampion, but DateTime<Utc> methods are accessible.
            entry: task.get_entry().map(|ts| ts.timestamp()),
            modified: task.get_modified().map(|ts| ts.timestamp()),
            due: task.get_due().map(|ts| ts.timestamp()),
            wait: task.get_wait().map(|ts| ts.timestamp()),
            parent: task.get_parent().map(|u| u.to_string()),
            position: task.get_position().map(|s| s.to_string()),
            tags: task
                .get_tags()
                .filter(|t| !t.is_synthetic())
                .map(|t| t.to_string())
                .collect(),
            annotations: task
                .get_annotations()
                .map(|a| FfiAnnotation {
                    entry: a.entry.timestamp(),
                    description: a.description.clone(),
                })
                .collect(),
            dependencies: task.get_dependencies().map(|u| u.to_string()).collect(),
            is_waiting: task.is_waiting(),
            is_active: task.is_active(),
            is_blocked: task.is_blocked(),
            is_blocking: task.is_blocking(),
        }
    }
}

// ---------------------------------------------------------------------------
// Status ↔ FfiStatus
// ---------------------------------------------------------------------------

impl From<Status> for FfiStatus {
    fn from(s: Status) -> Self {
        match s {
            Status::Pending => FfiStatus::Pending,
            Status::Completed => FfiStatus::Completed,
            Status::Deleted => FfiStatus::Deleted,
            Status::Recurring => FfiStatus::Recurring,
            Status::Unknown(v) => FfiStatus::Unknown { value: v },
        }
    }
}

impl From<FfiStatus> for Status {
    fn from(s: FfiStatus) -> Self {
        match s {
            FfiStatus::Pending => Status::Pending,
            FfiStatus::Completed => Status::Completed,
            FfiStatus::Deleted => Status::Deleted,
            FfiStatus::Recurring => Status::Recurring,
            FfiStatus::Unknown { value } => Status::Unknown(value),
        }
    }
}

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

impl From<taskchampion::Error> for FfiError {
    fn from(e: taskchampion::Error) -> Self {
        match e {
            taskchampion::Error::TaskNotFound(uuid) => FfiError::TaskNotFound {
                uuid: uuid.to_string(),
            },
            taskchampion::Error::TaskAlreadyExists(uuid) => FfiError::TaskAlreadyExists {
                uuid: uuid.to_string(),
            },
            taskchampion::Error::Database(msg) => FfiError::Storage { message: msg },
            taskchampion::Error::Usage(msg) => FfiError::InvalidInput { message: msg },
            taskchampion::Error::Other(e) => FfiError::Internal {
                message: e.to_string(),
            },
            // IMPORTANT: Error is #[non_exhaustive] — this catch-all is required
            // and must not be removed. Future core variants land here until
            // explicitly mapped.
            _ => FfiError::Internal {
                message: e.to_string(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// TreeMap → Vec<FfiTreeNode>
// ---------------------------------------------------------------------------

/// Convert a [`TreeMap`] to a flat list of [`FfiTreeNode`]s.
///
/// `position` is not available from `TreeMap` (it lives on `Task`), so it is
/// always `None`. Callers that need per-node position should overlay it from
/// the task list.
pub fn tree_map_to_ffi(tm: &TreeMap) -> Vec<FfiTreeNode> {
    let mut nodes = Vec::new();
    collect_tree(tm, None, &tm.roots(), &mut nodes);
    nodes
}

fn collect_tree(tm: &TreeMap, parent: Option<Uuid>, uuids: &[Uuid], nodes: &mut Vec<FfiTreeNode>) {
    for &uuid in uuids {
        let children = tm.children(uuid);
        let has_pending_children = !tm.pending_child_ids(uuid).is_empty();
        nodes.push(FfiTreeNode {
            uuid: uuid.to_string(),
            children: children.iter().map(|u| u.to_string()).collect(),
            parent: parent.map(|u| u.to_string()),
            position: None,
            is_pending: has_pending_children,
        });
        collect_tree(tm, Some(uuid), &children, nodes);
    }
}

// ---------------------------------------------------------------------------
// FfiSqlExecutor → SqlExecutor adapter
// ---------------------------------------------------------------------------

use crate::types::{FfiSqlExecutor, FfiSqlParam, FfiSqlStatement};
use std::sync::Arc;
use taskchampion::{SqlExecutor, SqlParam, SqlStatement};

/// Wraps a UniFFI callback interface (`Arc<dyn FfiSqlExecutor>`) and
/// implements the core [`SqlExecutor`] trait by converting between
/// owned FFI types and reference-based core types.
pub(crate) struct FfiSqlExecutorAdapter {
    inner: Arc<dyn FfiSqlExecutor>,
}

impl FfiSqlExecutorAdapter {
    pub(crate) fn new(executor: Arc<dyn FfiSqlExecutor>) -> Self {
        Self { inner: executor }
    }
}

#[async_trait]
impl SqlExecutor for FfiSqlExecutorAdapter {
    async fn query_one(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> std::result::Result<Option<String>, taskchampion::Error> {
        let ffi_params: Vec<FfiSqlParam> = params.iter().map(core_param_to_ffi).collect();
        self.inner
            .query_one(sql.to_string(), ffi_params)
            .await
            .map_err(ffi_error_to_core)
    }

    async fn query_all(
        &self,
        sql: &str,
        params: &[SqlParam],
    ) -> std::result::Result<Vec<String>, taskchampion::Error> {
        let ffi_params: Vec<FfiSqlParam> = params.iter().map(core_param_to_ffi).collect();
        self.inner
            .query_all(sql.to_string(), ffi_params)
            .await
            .map_err(ffi_error_to_core)
    }

    async fn execute_batch(
        &self,
        statements: &[SqlStatement],
    ) -> std::result::Result<(), taskchampion::Error> {
        let ffi_stmts: Vec<FfiSqlStatement> = statements.iter().map(core_stmt_to_ffi).collect();
        self.inner
            .execute_batch(ffi_stmts)
            .await
            .map_err(ffi_error_to_core)
    }
}

fn core_param_to_ffi(param: &SqlParam) -> FfiSqlParam {
    match param {
        SqlParam::Text(s) => FfiSqlParam::Text { value: s.clone() },
        SqlParam::Null => FfiSqlParam::Null,
    }
}

fn core_stmt_to_ffi(stmt: &SqlStatement) -> FfiSqlStatement {
    FfiSqlStatement {
        sql: stmt.sql.clone(),
        params: stmt.params.iter().map(core_param_to_ffi).collect(),
    }
}

/// Convert FFI errors back to core errors.
///
/// This is called by `FfiSqlExecutorAdapter` when the Swift-side SQL executor
/// returns an error. In practice, Swift should only return `Storage` or
/// `Internal` — `TaskNotFound` and `TaskAlreadyExists` flow Rust→Swift only.
/// However, the match must be exhaustive, so we handle them defensively with
/// best-effort UUID parsing.
fn ffi_error_to_core(e: FfiError) -> taskchampion::Error {
    match e {
        FfiError::TaskNotFound { uuid } => match uuid::Uuid::parse_str(&uuid) {
            Ok(u) => taskchampion::Error::TaskNotFound(u),
            Err(_) => taskchampion::Error::Database(format!("Task not found: {uuid}")),
        },
        FfiError::TaskAlreadyExists { uuid } => match uuid::Uuid::parse_str(&uuid) {
            Ok(u) => taskchampion::Error::TaskAlreadyExists(u),
            Err(_) => taskchampion::Error::Database(format!("Task already exists: {uuid}")),
        },
        FfiError::Storage { message } => taskchampion::Error::Database(message),
        FfiError::InvalidInput { message } => taskchampion::Error::Usage(message),
        FfiError::Internal { message } => {
            taskchampion::Error::Other(anyhow::anyhow!("{}", message))
        }
    }
}
