//! FFI type definitions for the TaskChampion UniFFI bridge.
//!
//! ## Naming Convention
//!
//! All public types use an `Ffi` prefix (e.g. `FfiTask`, `FfiStatus`) because
//! UniFFI 0.31 does not support `#[uniffi(name = "...")]` on derive macros.
//! Once upstream adds this (<https://github.com/mozilla/uniffi-rs/issues/2507>),
//! rename to `TC` prefix (`TCTask`, `TCStatus`, etc.) and add
//! `#[uniffi(name = "TC*")]` attributes.

/// Task status, mirroring `taskchampion::Status`.
#[derive(uniffi::Enum)]
pub enum FfiStatus {
    Pending,
    Completed,
    Deleted,
    Recurring,
    Unknown { value: String },
}

/// A single task annotation.
#[derive(uniffi::Record)]
pub struct FfiAnnotation {
    /// Unix epoch seconds.
    pub entry: i64,
    pub description: String,
}

/// Flat representation of a task suitable for FFI transfer.
#[derive(uniffi::Record)]
pub struct FfiTask {
    pub uuid: String,
    pub status: FfiStatus,
    pub description: String,
    pub priority: String,
    /// Unix epoch seconds, or `None` if not set.
    pub entry: Option<i64>,
    pub modified: Option<i64>,
    pub due: Option<i64>,
    pub wait: Option<i64>,
    /// Scheduled date as Unix epoch seconds, or `None` if not set.
    pub scheduled: Option<i64>,
    /// Start time (active tracking) as Unix epoch seconds, or `None`.
    pub start: Option<i64>,
    /// Parent task UUID as a string, or `None`.
    pub parent: Option<String>,
    pub position: Option<String>,
    /// User-visible tags (synthetic tags excluded).
    pub tags: Vec<String>,
    pub annotations: Vec<FfiAnnotation>,
    /// UUIDs of tasks this task depends on.
    pub dependencies: Vec<String>,
    pub is_waiting: bool,
    pub is_active: bool,
    pub is_blocked: bool,
    pub is_blocking: bool,
    /// FlickNote: whether this is a full-day task. Derived from UDA `is_full_day`.
    pub is_full_day: bool,
    /// FlickNote: time estimate in 15-minute boxes. Derived from UDA `estimate`.
    /// `None` if not set or not a valid u32.
    pub estimate: Option<u32>,
    /// Recurrence spec string (e.g. `"monthly"`, `"7d"`). `None` if not a recurring template.
    pub recur: Option<String>,
    /// Recurrence mask string (e.g. `"-+XW"`). `None` if not a recurring template.
    pub mask: Option<String>,
    /// Index into the parent mask for a recurring child. `None` if not a child.
    /// Parsed from the `imask` UDA string; `None` if missing or not a valid u32.
    pub imask: Option<u32>,
    /// Recurrence expiry as Unix epoch seconds. `None` if not set.
    /// Parsed from the `until` UDA string; `None` if missing or not a valid i64.
    pub until: Option<i64>,
    /// Extended status name (e.g. `"blocked"`), or `None` if not set.
    ///
    /// Stored as UDA `"xstatus"` in the task. Definitions live in `tc_config.xstatus`.
    pub xstatus: Option<String>,
    /// Project name (e.g. `"work"`), or `None` if unassigned.
    ///
    /// Resolved from the `projects` table JOIN at query time.
    pub project: Option<String>,
    /// Project UUID string, or `None` if unassigned.
    ///
    /// Raw value from `tc_tasks.project_id` — same JOIN as `project`.
    pub project_id: Option<String>,
    /// User-defined attributes not covered by dedicated fields.
    ///
    /// Keys are the raw TaskMap keys (e.g. `"custom_field"`).
    /// Values are the raw string values from the TaskMap.
    /// Empty if the task has no UDAs.
    ///
    /// Keys excluded from this map: `"scheduled"`, `"is_full_day"`, `"estimate"`,
    /// `"recur"`, `"mask"`, `"imask"`, `"until"`, `"xstatus"` — all have typed
    /// accessor fields above. See [`DEDICATED_UDA_FIELDS`] for the authoritative list.
    pub remaining_data: std::collections::HashMap<String, String>,
}

/// UDA keys that have dedicated typed fields on [`FfiTask`].
///
/// These keys are excluded from `FfiTask.remaining_data` and rejected by the
/// `SetValue` mutation. When adding a new dedicated field, update this list —
/// both `convert.rs` and `task_ops.rs` reference it.
pub(crate) const DEDICATED_UDA_FIELDS: &[&str] = &[
    "scheduled",
    "is_full_day",
    "estimate",
    "recur",
    "mask",
    "imask",
    "until",
    "xstatus",
];

/// A node in the task tree (parent/child hierarchy).
#[derive(uniffi::Record)]
pub struct FfiTreeNode {
    pub uuid: String,
    /// Direct child UUIDs.
    pub children: Vec<String>,
    pub parent: Option<String>,
    /// Always `None` when returned from `tree_map()` — position lives on the
    /// `Task`, not on the `TreeMap`. Cross-reference with `all_tasks()` to
    /// obtain per-node position values.
    pub position: Option<String>,
    /// `true` if the node has at least one pending child.
    pub is_pending: bool,
}

/// A dependency edge: `from_uuid` depends on `to_uuid`.
#[derive(uniffi::Record)]
pub struct FfiDependencyEdge {
    /// The task that has the dependency.
    pub from_uuid: String,
    /// The task being depended upon.
    pub to_uuid: String,
}

/// Enum of all supported task mutations.
///
/// Pass a `Vec<TaskMutation>` to `mutate_task` — all mutations are applied in
/// a single transaction with one undo point.
#[derive(uniffi::Enum)]
pub enum TaskMutation {
    SetDescription {
        value: String,
    },
    SetStatus {
        status: FfiStatus,
    },
    SetPriority {
        value: String,
    },
    /// `None` clears the field.
    SetDue {
        epoch: Option<i64>,
    },
    SetWait {
        epoch: Option<i64>,
    },
    SetEntry {
        epoch: Option<i64>,
    },
    SetParent {
        uuid: Option<String>,
    },
    SetPosition {
        value: Option<String>,
    },
    AddTag {
        tag: String,
    },
    RemoveTag {
        tag: String,
    },
    AddAnnotation {
        entry: i64,
        description: String,
    },
    RemoveAnnotation {
        entry: i64,
    },
    AddDependency {
        uuid: String,
    },
    RemoveDependency {
        uuid: String,
    },
    /// Mark the task as completed.
    Done,
    /// Start tracking active time.
    Start,
    /// Stop tracking active time.
    Stop,
    /// Soft-delete: sets status to `Deleted`.
    Delete,
    /// Set the scheduled date. `None` clears the field.
    SetScheduled {
        epoch: Option<i64>,
    },
    /// Set the start time to a specific epoch. `None` clears the field.
    ///
    /// Unlike `Start` (which sets to now) and `Stop` (which clears),
    /// this variant accepts an arbitrary timestamp.
    SetStart {
        epoch: Option<i64>,
    },
    /// Set FlickNote is_full_day flag.
    ///
    /// Stored as `"true"` in TaskMap when `true`, removed when `false`.
    SetIsFullDay {
        value: bool,
    },
    /// Set FlickNote time estimate (count of 15-minute boxes, must be >0).
    ///
    /// Stored as a decimal string in TaskMap (e.g. `"2"` = 30 minutes).
    /// Pass `None` to clear.
    SetEstimate {
        boxes: Option<u32>,
    },
    /// Set the recurrence spec string. `None` clears the field.
    ///
    /// Use for recurring templates (e.g. `"monthly"`, `"7d"`).
    SetRecur {
        value: Option<String>,
    },
    /// Set the recurrence mask string. `None` clears the field.
    SetMask {
        value: Option<String>,
    },
    /// Set the recurring child's index into the parent mask. `None` clears the field.
    SetImask {
        value: Option<u32>,
    },
    /// Set the recurrence expiry date. `None` clears the field.
    ///
    /// Stored as a Unix epoch seconds string in TaskMap.
    SetUntil {
        epoch: Option<i64>,
    },
    /// Set the project by name. `None` clears the project assignment.
    ///
    /// The storage layer resolves (or creates) the project UUID automatically.
    SetProject {
        value: Option<String>,
    },
    /// Set the project by UUID. `None` clears the project assignment.
    ///
    /// Unlike `SetProject` (which resolves by name), this writes the
    /// `project_id` column directly. The caller is responsible for
    /// passing a valid project UUID — no existence check is performed.
    SetProjectId {
        value: Option<String>,
    },
    /// Generic escape hatch for setting arbitrary UDA values.
    ///
    /// `key` is the raw TaskMap key. `value` is `None` to remove.
    /// Returns `InvalidInput` if `key` is a known TaskChampion property
    /// (use the dedicated variant instead).
    SetValue {
        key: String,
        value: Option<String>,
    },
}

/// Target position when reparenting a task.
///
/// Passed to `reparent()` to specify where the task should be inserted
/// among the new parent's children.
#[derive(uniffi::Enum)]
pub enum ReparentPosition {
    /// Insert as the first child of the new parent.
    Beginning,
    /// Insert as the last child of the new parent.
    End,
    /// Insert immediately after the sibling identified by `anchor` UUID.
    After { anchor: String },
    /// Insert immediately before the sibling identified by `anchor` UUID.
    Before { anchor: String },
}

/// Error type returned by all FFI functions.
///
/// Variants are designed for programmatic matching on the Swift/Kotlin side.
/// Each carries enough context for the host to decide on UX (retry, show
/// message, refresh cache, etc.) without parsing strings.
#[derive(Debug, uniffi::Error)]
pub enum FfiError {
    /// The referenced task does not exist.
    TaskNotFound { uuid: String },
    /// A task with this UUID already exists (create collision).
    TaskAlreadyExists { uuid: String },
    /// Caller-side validation error (bad UUID format, invalid tag, etc.).
    InvalidInput { message: String },
    /// Storage-layer error (SQL execution failure, connection issue).
    Storage { message: String },
    /// Unexpected internal error (bug, catch-all).
    Internal { message: String },
    /// Reparent would create a cycle (uuid cannot be a descendant of parent).
    CircularParent { uuid: String, parent: String },
    /// Reorder anchor is not under the same parent as the task.
    NotASibling { uuid: String, anchor: String },
    /// Reorder/reparent anchor exists in the DB but has no position field.
    ///
    /// Use a positioned task as anchor, or call `SetPosition` first.
    AnchorHasNoPosition { uuid: String },
    /// The referenced project does not exist (SetProject with unknown name).
    ProjectNotFound { name: String },
    /// delete_tag / rename_tag on a tag name not present in tc_config.
    TagNotFound { name: String },
    /// rename_tag target already exists in tc_config.
    TagAlreadyExists { name: String },
    /// set_xstatus with a name not in tc_config.xstatus definitions.
    UnknownXStatus { name: String },
}

impl std::fmt::Display for FfiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FfiError::TaskNotFound { uuid } => write!(f, "Task not found: {uuid}"),
            FfiError::TaskAlreadyExists { uuid } => write!(f, "Task already exists: {uuid}"),
            FfiError::InvalidInput { message } => write!(f, "Invalid input: {message}"),
            FfiError::Storage { message } => write!(f, "Storage error: {message}"),
            FfiError::Internal { message } => write!(f, "Internal error: {message}"),
            FfiError::CircularParent { uuid, parent } => {
                write!(
                    f,
                    "Circular parent: {uuid} cannot be a descendant of {parent}"
                )
            }
            FfiError::NotASibling { uuid, anchor } => {
                write!(
                    f,
                    "Not a sibling: {uuid} and {anchor} have different parents"
                )
            }
            FfiError::AnchorHasNoPosition { uuid } => {
                write!(f, "Anchor has no position: {uuid}")
            }
            FfiError::ProjectNotFound { name } => write!(f, "Project not found: {name}"),
            FfiError::TagNotFound { name } => write!(f, "Tag not found: {name}"),
            FfiError::TagAlreadyExists { name } => write!(f, "Tag already exists: {name}"),
            FfiError::UnknownXStatus { name } => write!(f, "Unknown xstatus: {name}"),
        }
    }
}

impl std::error::Error for FfiError {}

// ── External Storage FFI types ───────────────────────────────────────

/// SQL parameter for external storage queries.
#[derive(uniffi::Enum, Clone)]
pub enum FfiSqlParam {
    Text { value: String },
    Null,
}

/// A single SQL statement with parameters, for batched execution.
#[derive(uniffi::Record, Clone)]
pub struct FfiSqlStatement {
    pub sql: String,
    pub params: Vec<FfiSqlParam>,
}

/// A single value from a SQL result row.
///
/// Maps to SQLite's native storage classes. The host (Swift/Kotlin)
/// populates these using typed cursor accessors — no string coercion needed.
#[derive(uniffi::Enum, Clone, Debug, PartialEq)]
pub enum FfiSqlValue {
    /// Text (SQLite TEXT).
    Text { value: String },
    /// Integer (SQLite INTEGER).
    Integer { value: i64 },
    /// Floating-point (SQLite REAL).
    Real { value: f64 },
    /// NULL.
    Null,
}

/// A single row from a SQL result set.
///
/// Column names and values are parallel arrays — `values[i]` corresponds
/// to `columns[i]`.
#[derive(uniffi::Record, Clone)]
pub struct FfiSqlRow {
    /// Column names in SELECT order.
    pub columns: Vec<String>,
    /// Values in the same order as `columns`.
    pub values: Vec<FfiSqlValue>,
}

/// Callback interface for host-side SQL execution.
///
/// The host (Swift/Kotlin) implements this trait with native async/await.
/// TaskChampion calls these methods to read/write task data through the
/// host's database connection.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait FfiSqlExecutor: Send + Sync {
    /// Execute a read query returning at most one row as typed columns.
    /// Returns `None` if no rows match.
    async fn query_one(
        &self,
        sql: String,
        params: Vec<FfiSqlParam>,
    ) -> Result<Option<FfiSqlRow>, FfiError>;

    /// Execute a read query returning all matching rows as typed columns.
    async fn query_all(
        &self,
        sql: String,
        params: Vec<FfiSqlParam>,
    ) -> Result<Vec<FfiSqlRow>, FfiError>;

    /// Execute a batch of write statements atomically.
    /// The host MUST wrap these in a transaction.
    async fn execute_batch(&self, statements: Vec<FfiSqlStatement>) -> Result<(), FfiError>;
}
