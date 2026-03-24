//! End-to-end round-trip test exercising the FFI surface via a
//! MockFfiSqlExecutor backed by in-memory SQLite.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use taskchampion_ffi::{
    replica_ops::{all_tasks, create_task, get_task, pending_tasks, undo},
    task_ops::mutate_task,
    types::{FfiError, FfiSqlExecutor, FfiSqlParam, FfiSqlStatement, FfiStatus, TaskMutation},
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Mock FfiSqlExecutor
// ---------------------------------------------------------------------------

/// In-memory SQLite mock implementing the FfiSqlExecutor callback interface.
/// Proves the full FFI → ExternalStorage → SQLite round-trip works.
struct MockFfiSqlExecutor {
    conn: Mutex<Connection>,
}

impl MockFfiSqlExecutor {
    fn new() -> Self {
        let conn = Connection::open_in_memory().expect("in-memory connection");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tc_tasks (
                id TEXT PRIMARY KEY, user_id TEXT,
                data TEXT NOT NULL DEFAULT '{}', entry_at TEXT, status TEXT,
                description TEXT, priority TEXT, modified_at TEXT,
                due_at TEXT, scheduled_at TEXT, start_at TEXT, end_at TEXT,
                wait_at TEXT, parent_id TEXT, position TEXT, project_id TEXT
            );
            CREATE TABLE IF NOT EXISTS tc_operations (
                id TEXT PRIMARY KEY, user_id TEXT,
                data TEXT NOT NULL,
                created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
            );
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY, name TEXT, user_id TEXT,
                created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
            );
            CREATE TABLE IF NOT EXISTS tc_tags (
                id TEXT PRIMARY KEY, task_id TEXT NOT NULL,
                user_id TEXT, name TEXT NOT NULL, UNIQUE (task_id, name)
            );
            CREATE TABLE IF NOT EXISTS tc_annotations (
                id TEXT PRIMARY KEY, task_id TEXT NOT NULL,
                user_id TEXT, entry_at TEXT NOT NULL, description TEXT NOT NULL
            );",
        )
        .expect("create tables");
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Convert a rusqlite Row to a JSON object string.
    fn row_to_json(row: &rusqlite::Row, col_count: usize) -> rusqlite::Result<String> {
        use rusqlite::types::ValueRef;
        let mut map = serde_json::Map::new();
        for i in 0..col_count {
            let name = row.as_ref().column_name(i)?.to_string();
            let val = match row.get_ref(i)? {
                ValueRef::Text(b) => {
                    serde_json::Value::String(String::from_utf8_lossy(b).into_owned())
                }
                ValueRef::Integer(n) => serde_json::Value::Number(n.into()),
                ValueRef::Real(f) => serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
                ValueRef::Null => serde_json::Value::Null,
                ValueRef::Blob(_) => serde_json::Value::Null,
            };
            map.insert(name, val);
        }
        Ok(serde_json::Value::Object(map).to_string())
    }

    /// Convert FfiSqlParam to a rusqlite-compatible value.
    fn bind_params(params: &[FfiSqlParam]) -> Vec<Box<dyn rusqlite::types::ToSql>> {
        params
            .iter()
            .map(|p| -> Box<dyn rusqlite::types::ToSql> {
                match p {
                    FfiSqlParam::Text { value } => Box::new(value.clone()),
                    FfiSqlParam::Null => Box::new(rusqlite::types::Null),
                }
            })
            .collect()
    }
}

impl FfiSqlExecutor for MockFfiSqlExecutor {
    fn query_one(&self, sql: String, params: Vec<FfiSqlParam>) -> Result<Option<String>, FfiError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql).map_err(|e| FfiError::Database {
            message: format!("Prepare failed: {e}"),
        })?;
        let col_count = stmt.column_count();
        let bound = Self::bind_params(&params);
        let refs: Vec<&dyn rusqlite::types::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
        let result = stmt.query_row(&*refs, |row| Self::row_to_json(row, col_count));
        match result {
            Ok(json) => Ok(Some(json)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(FfiError::Database {
                message: format!("Query failed: {e}"),
            }),
        }
    }

    fn query_all(&self, sql: String, params: Vec<FfiSqlParam>) -> Result<Vec<String>, FfiError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql).map_err(|e| FfiError::Database {
            message: format!("Prepare failed: {e}"),
        })?;
        let col_count = stmt.column_count();
        let bound = Self::bind_params(&params);
        let refs: Vec<&dyn rusqlite::types::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(&*refs, |row| Self::row_to_json(row, col_count))
            .map_err(|e| FfiError::Database {
                message: format!("Query failed: {e}"),
            })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| FfiError::Database {
                message: format!("Row read failed: {e}"),
            })
    }

    fn execute_batch(&self, statements: Vec<FfiSqlStatement>) -> Result<(), FfiError> {
        let mut conn = self.conn.lock().unwrap();
        let txn = conn.transaction().map_err(|e| FfiError::Database {
            message: format!("Begin txn failed: {e}"),
        })?;
        for stmt in &statements {
            let bound = Self::bind_params(&stmt.params);
            let refs: Vec<&dyn rusqlite::types::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
            txn.execute(&stmt.sql, &*refs)
                .map_err(|e| FfiError::Database {
                    message: format!("Execute failed: {e} (sql: {})", stmt.sql),
                })?;
        }
        txn.commit().map_err(|e| FfiError::Database {
            message: format!("Commit failed: {e}"),
        })?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn make_executor() -> Arc<dyn FfiSqlExecutor> {
    Arc::new(MockFfiSqlExecutor::new())
}

const USER_ID: &str = "00000000-0000-0000-0000-000000000000";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_create_and_read() {
    let exec = make_executor();
    let uuid = Uuid::new_v4().to_string();

    let task = create_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        "Hello FFI".into(),
    )
    .expect("create_task");
    assert_eq!(task.description, "Hello FFI");
    assert!(matches!(task.status, FfiStatus::Pending));

    let fetched = get_task(Arc::clone(&exec), USER_ID.into(), uuid.clone())
        .expect("get_task")
        .expect("task should exist");
    assert_eq!(fetched.uuid, uuid);
    assert_eq!(fetched.description, "Hello FFI");
}

#[test]
fn test_mutate_description() {
    let exec = make_executor();
    let uuid = Uuid::new_v4().to_string();

    create_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        "Original".into(),
    )
    .expect("create");

    let updated = mutate_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::SetDescription {
            value: "Updated".into(),
        }],
    )
    .expect("mutate")
    .expect("task still exists");

    assert_eq!(updated.description, "Updated");
}

#[test]
fn test_pending_tasks() {
    let exec = make_executor();

    let uuid1 = Uuid::new_v4().to_string();
    let uuid2 = Uuid::new_v4().to_string();

    create_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid1.clone(),
        "Task 1".into(),
    )
    .expect("create 1");
    create_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid2.clone(),
        "Task 2".into(),
    )
    .expect("create 2");

    let pending = pending_tasks(Arc::clone(&exec), USER_ID.into()).expect("pending_tasks");
    let descs: Vec<&str> = pending.iter().map(|t| t.description.as_str()).collect();
    assert!(descs.contains(&"Task 1"), "Task 1 should be pending");
    assert!(descs.contains(&"Task 2"), "Task 2 should be pending");
}

#[test]
fn test_all_tasks_includes_completed() {
    let exec = make_executor();
    let uuid = Uuid::new_v4().to_string();

    create_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        "Complete me".into(),
    )
    .expect("create");
    mutate_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::Done],
    )
    .expect("done");

    let all = all_tasks(Arc::clone(&exec), USER_ID.into()).expect("all_tasks");
    let task = all
        .iter()
        .find(|t| t.uuid == uuid)
        .expect("task in all_tasks");
    assert!(matches!(task.status, FfiStatus::Completed));
}

#[test]
fn test_undo_reverses_last_mutation() {
    let exec = make_executor();
    let uuid = Uuid::new_v4().to_string();

    create_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        "Original".into(),
    )
    .expect("create");

    mutate_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::SetDescription {
            value: "Changed".into(),
        }],
    )
    .expect("mutate");

    let task = get_task(Arc::clone(&exec), USER_ID.into(), uuid.clone())
        .expect("get_task ok")
        .expect("task exists");
    assert_eq!(task.description, "Changed");

    let undone = undo(Arc::clone(&exec), USER_ID.into()).expect("undo must not error");
    assert!(undone, "undo should return true after mutation");

    let task = get_task(Arc::clone(&exec), USER_ID.into(), uuid.clone())
        .expect("get_task ok")
        .expect("task exists after undo");
    assert_eq!(task.description, "Original");
}

#[test]
fn test_add_and_remove_tag() {
    let exec = make_executor();
    let uuid = Uuid::new_v4().to_string();

    create_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        "Tag test".into(),
    )
    .expect("create");

    mutate_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::AddTag { tag: "work".into() }],
    )
    .expect("add tag");

    let with_tag = get_task(Arc::clone(&exec), USER_ID.into(), uuid.clone())
        .expect("get")
        .expect("exists");
    assert!(with_tag.tags.contains(&"work".to_string()));

    mutate_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::RemoveTag { tag: "work".into() }],
    )
    .expect("remove tag");

    let without_tag = get_task(Arc::clone(&exec), USER_ID.into(), uuid.clone())
        .expect("get")
        .expect("exists");
    assert!(!without_tag.tags.contains(&"work".to_string()));
}

#[test]
fn test_set_due_round_trip() {
    let exec = make_executor();
    let uuid = Uuid::new_v4().to_string();
    let epoch: i64 = 1_700_000_000;

    create_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        "Due test".into(),
    )
    .expect("create");

    mutate_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::SetDue { epoch: Some(epoch) }],
    )
    .expect("set due");

    let task = get_task(Arc::clone(&exec), USER_ID.into(), uuid.clone())
        .expect("get")
        .expect("exists");
    assert_eq!(task.due, Some(epoch), "due round-trip via set_value");

    mutate_task(
        Arc::clone(&exec),
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::SetDue { epoch: None }],
    )
    .expect("clear due");

    let cleared = get_task(Arc::clone(&exec), USER_ID.into(), uuid)
        .expect("get after clear")
        .expect("exists after clear");
    assert_eq!(cleared.due, None, "due should be None after clearing");
}
