/// Task status, mirroring `taskchampion::Status`.
// TODO: rename to TCStatus when UniFFI supports #[uniffi(name)] on derive macros
#[derive(uniffi::Enum)]
pub enum FfiStatus {
    Pending,
    Completed,
    Deleted,
    Recurring,
    Unknown { value: String },
}

/// A single task annotation.
// TODO: rename to TCAnnotation when UniFFI supports #[uniffi(name)] on derive macros
#[derive(uniffi::Record)]
pub struct FfiAnnotation {
    /// Unix epoch seconds.
    pub entry: i64,
    pub description: String,
}

/// Flat representation of a task suitable for FFI transfer.
// TODO: rename to TCTask when UniFFI supports #[uniffi(name)] on derive macros
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
}

/// A node in the task tree (parent/child hierarchy).
// TODO: rename to TCTreeNode when UniFFI supports #[uniffi(name)] on derive macros
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
// TODO: rename to TCDependencyEdge when UniFFI supports #[uniffi(name)] on derive macros
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
// TODO: rename to TCMutation when UniFFI supports #[uniffi(name)] on derive macros
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
}

/// Error type returned by all FFI functions.
///
/// Variants are designed for programmatic matching on the Swift/Kotlin side.
/// Each carries enough context for the host to decide on UX (retry, show
/// message, refresh cache, etc.) without parsing strings.
// TODO: rename to TCError when UniFFI supports #[uniffi(name)] on derive macros
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
}

impl std::fmt::Display for FfiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FfiError::TaskNotFound { uuid } => write!(f, "Task not found: {uuid}"),
            FfiError::TaskAlreadyExists { uuid } => write!(f, "Task already exists: {uuid}"),
            FfiError::InvalidInput { message } => write!(f, "Invalid input: {message}"),
            FfiError::Storage { message } => write!(f, "Storage error: {message}"),
            FfiError::Internal { message } => write!(f, "Internal error: {message}"),
        }
    }
}

impl std::error::Error for FfiError {}

// ── External Storage FFI types ───────────────────────────────────────

/// SQL parameter for external storage queries.
// TODO: rename to TCSqlParam when UniFFI supports #[uniffi(name)] on derive macros
#[derive(uniffi::Enum, Clone)]
pub enum FfiSqlParam {
    Text { value: String },
    Null,
}

/// A single SQL statement with parameters, for batched execution.
// TODO: rename to TCSqlStatement when UniFFI supports #[uniffi(name)] on derive macros
#[derive(uniffi::Record, Clone)]
pub struct FfiSqlStatement {
    pub sql: String,
    pub params: Vec<FfiSqlParam>,
}

/// A single value from a SQL result row.
///
/// Maps to SQLite's native storage classes. The host (Swift/Kotlin)
/// populates these using typed cursor accessors — no string coercion needed.
// TODO: rename to TCSqlValue when UniFFI supports #[uniffi(name)] on derive macros
#[derive(uniffi::Enum, Clone)]
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
// TODO: rename to TCSqlRow when UniFFI supports #[uniffi(name)] on derive macros
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
// TODO: rename to TCStorageExecutor when UniFFI supports name attr on callback interfaces
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
