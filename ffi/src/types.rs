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
}

/// Error type returned by all FFI functions.
#[derive(Debug, uniffi::Error)]
pub enum FfiError {
    Database { message: String },
    Usage { message: String },
    Internal { message: String },
}

impl std::fmt::Display for FfiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FfiError::Database { message } => write!(f, "Database error: {message}"),
            FfiError::Usage { message } => write!(f, "Usage error: {message}"),
            FfiError::Internal { message } => write!(f, "Internal error: {message}"),
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

/// Callback interface for host-side SQL execution.
///
/// The host (Swift/Kotlin) implements this trait. TaskChampion calls these
/// methods to read/write task data through the host's database connection.
#[uniffi::export(with_foreign)]
pub trait FfiSqlExecutor: Send + Sync {
    /// Execute a read query returning at most one row as a JSON object string.
    /// Returns `None` if no rows match.
    fn query_one(&self, sql: String, params: Vec<FfiSqlParam>) -> Result<Option<String>, FfiError>;

    /// Execute a read query returning all matching rows as JSON object strings.
    fn query_all(&self, sql: String, params: Vec<FfiSqlParam>) -> Result<Vec<String>, FfiError>;

    /// Execute a batch of write statements atomically.
    /// The host MUST wrap these in a transaction.
    fn execute_batch(&self, statements: Vec<FfiSqlStatement>) -> Result<(), FfiError>;
}
