//! Pure SQL generation — no connections, no side effects.
//!
//! Both `PowerSyncTxn` and `ExternalStorageTxn` use these functions to
//! produce SQL statements. The caller decides how to execute them.

use chrono::Utc;
use uuid::Uuid;

use crate::errors::{Error, Result};
use crate::operation::Operation;
use crate::storage::columns::extract_timestamp;
use crate::storage::TaskMap;

/// A single SQL statement with bound parameters.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "storage-external"), allow(unreachable_pub))]
pub struct SqlStatement {
    pub sql: String,
    pub params: Vec<SqlParam>,
}

/// Parameter types for SQL statements.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "storage-external"), allow(unreachable_pub))]
pub enum SqlParam {
    Text(String),
    Null,
}

/// Allow `SqlParam` values to be used directly with rusqlite execute/query calls.
#[cfg(feature = "storage-powersync")]
impl rusqlite::types::ToSql for SqlParam {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            SqlParam::Text(s) => s.to_sql(),
            SqlParam::Null => rusqlite::types::Null.to_sql(),
        }
    }
}

/// Task data parsed into promoted columns and residual blob.
/// Tags and annotations remain in the data blob as `tag_*` / `annotation_*` keys.
pub(crate) struct PreparedTask {
    pub(crate) data_json: String,
    pub(crate) status: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) priority: Option<String>,
    pub(crate) parent_id: Option<String>,
    pub(crate) position: Option<String>,
    pub(crate) entry_at: Option<String>,
    pub(crate) modified_at: Option<String>,
    pub(crate) due_at: Option<String>,
    pub(crate) scheduled_at: Option<String>,
    pub(crate) start_at: Option<String>,
    pub(crate) end_at: Option<String>,
    pub(crate) wait_at: Option<String>,
    pub(crate) project_name: Option<String>,
}

/// Parse a TaskMap into its promoted columns and residual data blob.
/// Tags and annotations are left in `task_data` and serialized into `data_json`.
pub(crate) fn prepare_task(mut task_data: TaskMap) -> Result<PreparedTask> {
    // Extract string columns.
    let status = task_data.remove("status");
    let description = task_data.remove("description");
    let priority = task_data.remove("priority");
    let parent_id = task_data.remove("parent_id");
    let position = task_data.remove("position");

    // Extract timestamps (epoch → ISO).
    let entry_at = extract_timestamp(&mut task_data, "entry")?;
    let modified_at = extract_timestamp(&mut task_data, "modified")?;
    let due_at = extract_timestamp(&mut task_data, "due")?;
    let scheduled_at = extract_timestamp(&mut task_data, "scheduled")?;
    let start_at = extract_timestamp(&mut task_data, "start")?;
    let end_at = extract_timestamp(&mut task_data, "end")?;
    let wait_at = extract_timestamp(&mut task_data, "wait")?;

    // Extract project name.
    let project_name = task_data.remove("project");

    // Validate annotation keys have integer epoch suffixes.
    for k in task_data.keys().filter(|k| k.starts_with("annotation_")) {
        let epoch_str = k.strip_prefix("annotation_").unwrap();
        epoch_str.parse::<i64>().map_err(|_| {
            Error::Database(format!(
                "Invalid annotation key {k:?}: epoch suffix is not an integer"
            ))
        })?;
    }

    // Tags and annotations remain in task_data and are serialized into the blob.
    let data_json = serde_json::to_string(&task_data)
        .map_err(|e| Error::Database(format!("Failed to serialize task data: {e}")))?;

    Ok(PreparedTask {
        data_json,
        status,
        description,
        priority,
        parent_id,
        position,
        entry_at,
        modified_at,
        due_at,
        scheduled_at,
        start_at,
        end_at,
        wait_at,
        project_name,
    })
}

/// Helper: convert an Option<String> to SqlParam.
fn opt(v: &Option<String>) -> SqlParam {
    match v {
        Some(s) => SqlParam::Text(s.clone()),
        None => SqlParam::Null,
    }
}

/// Generate SQL statements for set_task (INSERT or UPDATE).
pub(crate) fn set_task_stmts(
    uuid: &Uuid,
    prepared: &PreparedTask,
    exists: bool,
    project_id: Option<&str>,
) -> Result<Vec<SqlStatement>> {
    let mut stmts = Vec::new();
    let uuid_str = uuid.to_string();
    let project_param = project_id
        .map(|s| SqlParam::Text(s.to_string()))
        .unwrap_or(SqlParam::Null);

    if exists {
        stmts.push(SqlStatement {
            sql: "UPDATE tc_tasks SET \
                  data = ?, status = ?, description = ?, priority = ?, \
                  entry_at = ?, modified_at = ?, due_at = ?, scheduled_at = ?, \
                  start_at = ?, end_at = ?, wait_at = ?, parent_id = ?, position = ?, project_id = ? \
                  WHERE id = ?"
                .into(),
            params: vec![
                SqlParam::Text(prepared.data_json.clone()),
                opt(&prepared.status),
                opt(&prepared.description),
                opt(&prepared.priority),
                opt(&prepared.entry_at),
                opt(&prepared.modified_at),
                opt(&prepared.due_at),
                opt(&prepared.scheduled_at),
                opt(&prepared.start_at),
                opt(&prepared.end_at),
                opt(&prepared.wait_at),
                opt(&prepared.parent_id),
                opt(&prepared.position),
                project_param,
                SqlParam::Text(uuid_str),
            ],
        });
    } else {
        stmts.push(SqlStatement {
            sql: "INSERT INTO tc_tasks \
                  (id, data, status, description, priority, \
                   entry_at, modified_at, due_at, scheduled_at, start_at, end_at, wait_at, \
                   parent_id, position, project_id) \
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                .into(),
            params: vec![
                SqlParam::Text(uuid_str),
                SqlParam::Text(prepared.data_json.clone()),
                opt(&prepared.status),
                opt(&prepared.description),
                opt(&prepared.priority),
                opt(&prepared.entry_at),
                opt(&prepared.modified_at),
                opt(&prepared.due_at),
                opt(&prepared.scheduled_at),
                opt(&prepared.start_at),
                opt(&prepared.end_at),
                opt(&prepared.wait_at),
                opt(&prepared.parent_id),
                opt(&prepared.position),
                project_param,
            ],
        });
    }

    Ok(stmts)
}

/// Generate SQL statement for create_task (empty task).
pub(crate) fn create_task_stmt(uuid: &Uuid) -> SqlStatement {
    SqlStatement {
        sql: "INSERT INTO tc_tasks (id, data) VALUES (?, '{}')".into(),
        params: vec![SqlParam::Text(uuid.to_string())],
    }
}

/// Generate SQL statements for delete_task (task row only; tags/annotations are in the data blob).
pub(crate) fn delete_task_stmts(uuid: &Uuid) -> Vec<SqlStatement> {
    let uuid_str = uuid.to_string();
    vec![SqlStatement {
        sql: "DELETE FROM tc_tasks WHERE id = ?".into(),
        params: vec![SqlParam::Text(uuid_str)],
    }]
}

/// Generate SQL statement for add_operation.
pub(crate) fn add_operation_stmt(op: &Operation) -> Result<SqlStatement> {
    let created_at = match op {
        Operation::Update { timestamp, .. } => {
            timestamp.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
        }
        _ => Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
    };
    let data_str = serde_json::to_string(op)
        .map_err(|e| Error::Database(format!("Failed to serialize operation: {e}")))?;
    Ok(SqlStatement {
        sql: "INSERT INTO tc_operations (id, data, created_at) VALUES (?, ?, ?)".into(),
        params: vec![
            SqlParam::Text(Uuid::now_v7().to_string()),
            SqlParam::Text(data_str),
            SqlParam::Text(created_at),
        ],
    })
}

/// Generate SQL statement for remove_operation (by row ID).
pub(crate) fn remove_operation_stmt(id: &str) -> SqlStatement {
    SqlStatement {
        sql: "DELETE FROM tc_operations WHERE id = ?".into(),
        params: vec![SqlParam::Text(id.to_string())],
    }
}

/// Generate a SQL statement for setting a tag color.
///
/// If `existing_id` is `Some`, updates the existing row. Otherwise, inserts a new row.
pub(crate) fn set_tag_color_stmt(
    name: &str,
    color: &str,
    existing_id: Option<&str>,
) -> SqlStatement {
    match existing_id {
        Some(id) => SqlStatement {
            sql: "UPDATE tc_tag_colors SET color = ? WHERE id = ?".into(),
            params: vec![
                SqlParam::Text(color.to_string()),
                SqlParam::Text(id.to_string()),
            ],
        },
        None => SqlStatement {
            sql: "INSERT INTO tc_tag_colors (id, name, color) VALUES (?, ?, ?)".into(),
            params: vec![
                SqlParam::Text(Uuid::now_v7().to_string()),
                SqlParam::Text(name.to_string()),
                SqlParam::Text(color.to_string()),
            ],
        },
    }
}

/// Generate SQL statement for inserting a new project.
#[cfg(feature = "storage-external")]
pub(crate) fn insert_project_stmt(id: &Uuid, name: &str) -> SqlStatement {
    SqlStatement {
        sql: "INSERT OR IGNORE INTO projects (id, name) VALUES (?, ?)".into(),
        params: vec![
            SqlParam::Text(id.to_string()),
            SqlParam::Text(name.to_string()),
        ],
    }
}

// ── Read SQL constants ─────────────────────────────────────────────────────

pub(crate) const TASK_EXISTS_SQL: &str =
    "SELECT EXISTS(SELECT 1 FROM tc_tasks WHERE id = ?) AS exists_flag";
#[cfg(feature = "storage-external")]
pub(crate) const PROJECT_LOOKUP_SQL: &str =
    "SELECT id FROM projects WHERE name = ? ORDER BY created_at LIMIT 1";
pub(crate) const ALL_OPERATIONS_SQL: &str = "SELECT data FROM tc_operations ORDER BY id ASC";
#[cfg(feature = "storage-external")]
pub(crate) const ALL_OPS_WITH_ID_DESC_SQL: &str =
    "SELECT id, data FROM tc_operations ORDER BY id DESC";
pub(crate) const LAST_OPERATION_SQL: &str =
    "SELECT id, data FROM tc_operations ORDER BY id DESC LIMIT 1";
pub(crate) const ALL_TASK_UUIDS_SQL: &str = "SELECT id FROM tc_tasks";
pub(crate) const TAG_COLOR_READ_SQL: &str =
    "SELECT id, color FROM tc_tag_colors WHERE name = ? ORDER BY created_at DESC LIMIT 1";
pub(crate) const ALL_TAGS_SQL: &str = "SELECT DISTINCT substr(j.key, 5) as name \
     FROM tc_tasks, json_each(tc_tasks.data) as j \
     WHERE j.key LIKE 'tag_%' \
     ORDER BY name";
