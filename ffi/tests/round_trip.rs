//! End-to-end round-trip test exercising the FFI surface via a
//! MockFfiSqlExecutor backed by in-memory SQLite.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use taskchampion::position::sequential_positions;
use taskchampion_ffi::{
    replica_ops::FfiSession,
    types::{
        FfiError, FfiSqlExecutor, FfiSqlParam, FfiSqlRow, FfiSqlStatement, FfiSqlValue, FfiStatus,
        ReparentPosition, TaskMutation,
    },
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
                id TEXT PRIMARY KEY,
                data TEXT NOT NULL DEFAULT '{}', entry_at TEXT, status TEXT,
                description TEXT, priority TEXT, modified_at TEXT,
                due_at TEXT, scheduled_at TEXT, start_at TEXT, end_at TEXT,
                wait_at TEXT, parent_id TEXT, position TEXT, project_id TEXT
            );
            CREATE TABLE IF NOT EXISTS tc_operations (
                id TEXT PRIMARY KEY,
                data TEXT NOT NULL,
                created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
            );
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY, name TEXT,
                created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
            );
            CREATE TABLE IF NOT EXISTS tc_settings (
                id TEXT PRIMARY KEY,
                key TEXT NOT NULL,
                value TEXT NOT NULL DEFAULT '{}'
            );",
        )
        .expect("create tables");
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Convert a rusqlite Row to an FfiSqlRow with typed values.
    fn row_to_ffi(row: &rusqlite::Row, col_count: usize) -> rusqlite::Result<FfiSqlRow> {
        use rusqlite::types::ValueRef;
        let mut columns = Vec::with_capacity(col_count);
        let mut values = Vec::with_capacity(col_count);
        for i in 0..col_count {
            columns.push(row.as_ref().column_name(i)?.to_string());
            let val = match row.get_ref(i)? {
                ValueRef::Text(b) => FfiSqlValue::Text {
                    value: String::from_utf8_lossy(b).into_owned(),
                },
                ValueRef::Integer(n) => FfiSqlValue::Integer { value: n },
                ValueRef::Real(f) => FfiSqlValue::Real { value: f },
                ValueRef::Null => FfiSqlValue::Null,
                // Blob columns are not used by task data; treat as NULL rather than panicking.
                ValueRef::Blob(_) => FfiSqlValue::Null,
            };
            values.push(val);
        }
        Ok(FfiSqlRow { columns, values })
    }

    /// Inject a tc_config JSON value into tc_settings for testing.
    fn inject_tc_config(&self, json: &str) {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO tc_settings (id, key, value) VALUES ('tc_config', 'tc_config', ?)",
            rusqlite::params![json],
        )
        .expect("inject_tc_config");
    }

    /// Read the current tc_config JSON value from tc_settings.
    fn read_tc_config(&self) -> String {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT value FROM tc_settings WHERE id = 'tc_config'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default()
    }

    /// Insert a project into the projects table and return its UUID string.
    fn inject_project(&self, name: &str) -> String {
        let conn = self.conn.lock().unwrap();
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO projects (id, name) VALUES (?, ?)",
            rusqlite::params![&id, name],
        )
        .expect("inject_project");
        id
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

#[async_trait::async_trait]
impl FfiSqlExecutor for MockFfiSqlExecutor {
    async fn query_one(
        &self,
        sql: String,
        params: Vec<FfiSqlParam>,
    ) -> Result<Option<FfiSqlRow>, FfiError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql).map_err(|e| FfiError::Storage {
            message: format!("Prepare failed: {e}"),
        })?;
        let col_count = stmt.column_count();
        let bound = Self::bind_params(&params);
        let refs: Vec<&dyn rusqlite::types::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
        let result = stmt.query_row(&*refs, |row| Self::row_to_ffi(row, col_count));
        match result {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(FfiError::Storage {
                message: format!("Query failed: {e}"),
            }),
        }
    }

    async fn query_all(
        &self,
        sql: String,
        params: Vec<FfiSqlParam>,
    ) -> Result<Vec<FfiSqlRow>, FfiError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql).map_err(|e| FfiError::Storage {
            message: format!("Prepare failed: {e}"),
        })?;
        let col_count = stmt.column_count();
        let bound = Self::bind_params(&params);
        let refs: Vec<&dyn rusqlite::types::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(&*refs, |row| Self::row_to_ffi(row, col_count))
            .map_err(|e| FfiError::Storage {
                message: format!("Query failed: {e}"),
            })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| FfiError::Storage {
                message: format!("Row read failed: {e}"),
            })
    }

    async fn execute_batch(&self, statements: Vec<FfiSqlStatement>) -> Result<(), FfiError> {
        let mut conn = self.conn.lock().unwrap();
        let txn = conn.transaction().map_err(|e| FfiError::Storage {
            message: format!("Begin txn failed: {e}"),
        })?;
        for stmt in &statements {
            let bound = Self::bind_params(&stmt.params);
            let refs: Vec<&dyn rusqlite::types::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
            txn.execute(&stmt.sql, &*refs)
                .map_err(|e| FfiError::Storage {
                    message: format!("Execute failed: {e} (sql: {})", stmt.sql),
                })?;
        }
        txn.commit().map_err(|e| FfiError::Storage {
            message: format!("Commit failed: {e}"),
        })?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn make_session() -> Arc<FfiSession> {
    let executor: Arc<dyn FfiSqlExecutor> = Arc::new(MockFfiSqlExecutor::new());
    FfiSession::new(executor)
}

/// Returns both the session and the underlying executor so tests can inject raw rows.
fn make_session_with_executor() -> (Arc<FfiSession>, Arc<MockFfiSqlExecutor>) {
    let mock = Arc::new(MockFfiSqlExecutor::new());
    let session = FfiSession::new(mock.clone() as Arc<dyn FfiSqlExecutor>);
    (session, mock)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_and_read() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    let task = session
        .create_task(uuid.clone(), "Hello FFI".into())
        .await
        .expect("create_task");
    assert_eq!(task.description, "Hello FFI");
    assert!(matches!(task.status, FfiStatus::Pending));

    let fetched = session
        .get_task(uuid.clone())
        .await
        .expect("get_task")
        .expect("task should exist");
    assert_eq!(fetched.uuid, uuid);
    assert_eq!(fetched.description, "Hello FFI");
}

#[tokio::test]
async fn test_mutate_description() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Original".into())
        .await
        .expect("create");

    let updated = session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetDescription {
                value: "Updated".into(),
            }],
        )
        .await
        .expect("mutate")
        .expect("task still exists");

    assert_eq!(updated.description, "Updated");
}

#[tokio::test]
async fn test_pending_tasks() {
    let session = make_session();

    let uuid1 = Uuid::new_v4().to_string();
    let uuid2 = Uuid::new_v4().to_string();

    session
        .create_task(uuid1.clone(), "Task 1".into())
        .await
        .expect("create 1");
    session
        .create_task(uuid2.clone(), "Task 2".into())
        .await
        .expect("create 2");

    let pending = session.pending_tasks().await.expect("pending_tasks");
    let descs: Vec<&str> = pending.iter().map(|t| t.description.as_str()).collect();
    assert!(descs.contains(&"Task 1"), "Task 1 should be pending");
    assert!(descs.contains(&"Task 2"), "Task 2 should be pending");
}

#[tokio::test]
async fn test_all_tasks_includes_completed() {
    let session = make_session();
    let uuid1 = Uuid::new_v4().to_string();
    let uuid2 = Uuid::new_v4().to_string();

    session
        .create_task(uuid1.clone(), "Task one".into())
        .await
        .expect("create 1");
    session
        .create_task(uuid2.clone(), "Complete me".into())
        .await
        .expect("create 2");
    session
        .mutate_task(uuid2.clone(), vec![TaskMutation::Done])
        .await
        .expect("done");

    let all = session.all_tasks().await.expect("all_tasks");
    assert!(all.len() >= 2, "should have at least 2 tasks");

    let task1 = all
        .iter()
        .find(|t| t.uuid == uuid1)
        .expect("task1 in all_tasks");
    assert!(matches!(task1.status, FfiStatus::Pending));

    let task2 = all
        .iter()
        .find(|t| t.uuid == uuid2)
        .expect("task2 in all_tasks");
    assert!(matches!(task2.status, FfiStatus::Completed));
}

#[tokio::test]
async fn test_undo_reverses_last_mutation() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Original".into())
        .await
        .expect("create");

    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetDescription {
                value: "Changed".into(),
            }],
        )
        .await
        .expect("mutate");

    let task = session
        .get_task(uuid.clone())
        .await
        .expect("get_task ok")
        .expect("task exists");
    assert_eq!(task.description, "Changed");

    let undone = session.undo().await.expect("undo must not error");
    assert!(undone, "undo should return true after mutation");

    let task = session
        .get_task(uuid.clone())
        .await
        .expect("get_task ok")
        .expect("task exists after undo");
    assert_eq!(task.description, "Original");
}

#[tokio::test]
async fn test_add_and_remove_tag() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"tags":"work"}"#);
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Tag test".into())
        .await
        .expect("create");

    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::AddTag { tag: "work".into() }],
        )
        .await
        .expect("add tag");

    let with_tag = session
        .get_task(uuid.clone())
        .await
        .expect("get")
        .expect("exists");
    assert!(with_tag.tags.contains(&"work".to_string()));

    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::RemoveTag { tag: "work".into() }],
        )
        .await
        .expect("remove tag");

    let without_tag = session
        .get_task(uuid.clone())
        .await
        .expect("get")
        .expect("exists");
    assert!(!without_tag.tags.contains(&"work".to_string()));
}

#[tokio::test]
async fn test_set_due_round_trip() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();
    let epoch: i64 = 1_700_000_000;

    session
        .create_task(uuid.clone(), "Due test".into())
        .await
        .expect("create");

    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetDue { epoch: Some(epoch) }],
        )
        .await
        .expect("set due");

    let task = session
        .get_task(uuid.clone())
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(task.due, Some(epoch), "due round-trip via set_value");

    session
        .mutate_task(uuid.clone(), vec![TaskMutation::SetDue { epoch: None }])
        .await
        .expect("clear due");

    let cleared = session
        .get_task(uuid)
        .await
        .expect("get after clear")
        .expect("exists after clear");
    assert_eq!(cleared.due, None, "due should be None after clearing");
}

#[tokio::test]
async fn test_tree_map_parent_child() {
    let session = make_session();
    let parent_uuid = Uuid::new_v4().to_string();
    let child_uuid = Uuid::new_v4().to_string();

    session
        .create_task(parent_uuid.clone(), "Parent".into())
        .await
        .expect("create parent");
    session
        .create_task(child_uuid.clone(), "Child".into())
        .await
        .expect("create child");
    session
        .mutate_task(
            child_uuid.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent_uuid.clone()),
            }],
        )
        .await
        .expect("set parent");

    let nodes = session.tree_map().await.expect("tree_map");
    let parent_node = nodes
        .iter()
        .find(|n| n.uuid == parent_uuid)
        .expect("parent in tree");
    assert!(
        parent_node.children.contains(&child_uuid),
        "child in parent's children"
    );

    let child_node = nodes
        .iter()
        .find(|n| n.uuid == child_uuid)
        .expect("child in tree");
    assert_eq!(child_node.parent.as_deref(), Some(parent_uuid.as_str()));
}

#[tokio::test]
async fn test_dependency_map_edge() {
    let session = make_session();
    let task_a = Uuid::new_v4().to_string();
    let task_b = Uuid::new_v4().to_string();

    session
        .create_task(task_a.clone(), "Task A".into())
        .await
        .expect("create A");
    session
        .create_task(task_b.clone(), "Task B".into())
        .await
        .expect("create B");
    session
        .mutate_task(
            task_a.clone(),
            vec![TaskMutation::AddDependency {
                uuid: task_b.clone(),
            }],
        )
        .await
        .expect("add dep");

    let edges = session.dependency_map().await.expect("dependency_map");
    let edge = edges
        .iter()
        .find(|e| e.from_uuid == task_a && e.to_uuid == task_b);
    assert!(edge.is_some(), "dependency edge A→B should exist");
}

#[tokio::test]
async fn test_create_duplicate_returns_task_already_exists() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "First".into())
        .await
        .expect("first create");

    match session.create_task(uuid.clone(), "Duplicate".into()).await {
        Ok(_) => panic!("duplicate create should have failed"),
        Err(err) => assert!(
            matches!(err, FfiError::TaskAlreadyExists { .. }),
            "Expected TaskAlreadyExists, got: {err:?}"
        ),
    }
}

#[tokio::test]
async fn test_position_numeric_string_round_trip() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Position test".into())
        .await
        .expect("create");

    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetPosition {
                value: Some("80".into()),
            }],
        )
        .await
        .expect("set position");

    let task = session.get_task(uuid).await.expect("get").expect("exists");
    assert_eq!(
        task.position.as_deref(),
        Some("80"),
        "numeric position string must survive round-trip"
    );
}

#[tokio::test]
async fn test_delete_tag_removes_from_config_and_tasks() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"tags":"work,home"}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Tag delete test".into())
        .await
        .unwrap();
    session
        .mutate_task(
            uuid.clone(),
            vec![
                TaskMutation::AddTag { tag: "work".into() },
                TaskMutation::AddTag { tag: "home".into() },
            ],
        )
        .await
        .unwrap();

    let count = session.delete_tag("work".into()).await.unwrap();
    assert_eq!(count, 1, "one task had 'work' removed");

    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert!(!task.tags.contains(&"work".to_string()), "work tag removed");
    assert!(task.tags.contains(&"home".to_string()), "home tag intact");
}

#[tokio::test]
async fn test_delete_tag_not_found() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"tags":"work"}"#);

    let result = session.delete_tag("ghost".into()).await;
    assert!(
        matches!(result, Err(FfiError::TagNotFound { .. })),
        "expected TagNotFound, got: {result:?}"
    );
}

#[tokio::test]
async fn test_rename_tag_success() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"tags":"oldtag,home"}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Rename test".into())
        .await
        .unwrap();
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::AddTag {
                tag: "oldtag".into(),
            }],
        )
        .await
        .unwrap();

    let count = session
        .rename_tag("oldtag".into(), "newtag".into())
        .await
        .unwrap();
    assert_eq!(count, 1, "one task had tag renamed");

    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert!(task.tags.contains(&"newtag".to_string()));
    assert!(!task.tags.contains(&"oldtag".to_string()));
}

#[tokio::test]
async fn test_rename_tag_not_found() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"tags":"work"}"#);

    let result = session.rename_tag("ghost".into(), "other".into()).await;
    assert!(
        matches!(result, Err(FfiError::TagNotFound { .. })),
        "expected TagNotFound, got: {result:?}"
    );
}

#[tokio::test]
async fn test_rename_tag_already_exists() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"tags":"old,new"}"#);

    let result = session.rename_tag("old".into(), "new".into()).await;
    assert!(
        matches!(result, Err(FfiError::TagAlreadyExists { .. })),
        "expected TagAlreadyExists, got: {result:?}"
    );
}

#[tokio::test]
async fn test_xstatus_set_and_clear() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"xstatus":[{"name":"blocked","icon":128721}]}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Xstatus test".into())
        .await
        .unwrap();

    // Set xstatus.
    let task = session
        .set_xstatus(uuid.clone(), "blocked".into())
        .await
        .unwrap();
    assert_eq!(task.xstatus.as_deref(), Some("blocked"));
    assert!(matches!(task.status, FfiStatus::Pending));

    // Clear xstatus.
    let task = session.clear_xstatus(uuid.clone()).await.unwrap();
    assert_eq!(task.xstatus, None);
    assert!(matches!(task.status, FfiStatus::Pending));
}

#[tokio::test]
async fn test_xstatus_unknown_name_rejected() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"xstatus":[{"name":"blocked","icon":128721}]}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Unknown xstatus".into())
        .await
        .unwrap();

    let result = session
        .set_xstatus(uuid.clone(), "nonexistent".into())
        .await;
    assert!(
        matches!(result, Err(FfiError::UnknownXStatus { .. })),
        "expected UnknownXStatus"
    );
}

#[tokio::test]
async fn test_xstatus_auto_clears_on_done() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"xstatus":[{"name":"blocked","icon":128721}]}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Auto clear test".into())
        .await
        .unwrap();

    session
        .set_xstatus(uuid.clone(), "blocked".into())
        .await
        .unwrap();

    // Mark done — xstatus should auto-clear.
    let task = session
        .mutate_task(uuid.clone(), vec![TaskMutation::Done])
        .await
        .unwrap()
        .unwrap();

    assert_eq!(task.xstatus, None, "xstatus must clear on Done");
    assert!(matches!(task.status, FfiStatus::Completed));
}

#[tokio::test]
async fn test_xstatus_auto_clears_on_delete() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"xstatus":[{"name":"blocked","icon":128721}]}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Auto clear delete test".into())
        .await
        .unwrap();

    session
        .set_xstatus(uuid.clone(), "blocked".into())
        .await
        .unwrap();

    // Soft delete — xstatus should auto-clear.
    let task = session
        .mutate_task(uuid.clone(), vec![TaskMutation::Delete])
        .await
        .unwrap()
        .unwrap();

    assert_eq!(task.xstatus, None, "xstatus must clear on Delete");
    assert!(matches!(task.status, FfiStatus::Deleted));
}

#[tokio::test]
async fn test_xstatus_set_on_non_pending_restores_pending() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"xstatus":[{"name":"blocked","icon":128721}]}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Non-pending restore test".into())
        .await
        .unwrap();

    // Complete the task.
    session
        .mutate_task(uuid.clone(), vec![TaskMutation::Done])
        .await
        .unwrap();

    // set_xstatus on a completed task should restore it to pending.
    let task = session
        .set_xstatus(uuid.clone(), "blocked".into())
        .await
        .unwrap();

    assert!(
        matches!(task.status, FfiStatus::Pending),
        "set_xstatus should restore pending status"
    );
    assert_eq!(task.xstatus.as_deref(), Some("blocked"));
}

#[tokio::test]
async fn test_xstatus_not_in_remaining_data() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"xstatus":[{"name":"blocked","icon":128721}]}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Remaining data test".into())
        .await
        .unwrap();

    session
        .set_xstatus(uuid.clone(), "blocked".into())
        .await
        .unwrap();

    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert!(
        !task.remaining_data.contains_key("xstatus"),
        "xstatus must not appear in remaining_data"
    );
}

#[tokio::test]
async fn test_scheduled_round_trip() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();
    let epoch: i64 = 1_700_000_000;

    session
        .create_task(uuid.clone(), "Scheduled test".into())
        .await
        .expect("create");

    // Default: None
    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.scheduled, None);

    // Set scheduled
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetScheduled { epoch: Some(epoch) }],
        )
        .await
        .expect("set scheduled");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.scheduled, Some(epoch));
    // "scheduled" should NOT appear in remaining_data
    assert!(!task.remaining_data.contains_key("scheduled"));

    // Clear scheduled
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetScheduled { epoch: None }],
        )
        .await
        .expect("clear scheduled");

    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert_eq!(task.scheduled, None);
}

#[tokio::test]
async fn test_start_epoch_round_trip() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();
    let epoch: i64 = 1_700_000_000;

    session
        .create_task(uuid.clone(), "Start test".into())
        .await
        .expect("create");

    // Default: None
    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.start, None);

    // Set start to specific epoch
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetStart { epoch: Some(epoch) }],
        )
        .await
        .expect("set start");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.start, Some(epoch));

    // Clear via SetStart { epoch: None }
    session
        .mutate_task(uuid.clone(), vec![TaskMutation::SetStart { epoch: None }])
        .await
        .expect("clear start");

    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert_eq!(task.start, None);
}

#[tokio::test]
async fn test_is_full_day_round_trip() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Full day test".into())
        .await
        .expect("create");

    // Default: false
    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert!(!task.is_full_day, "default should be false");

    // Set full day
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetIsFullDay { value: true }],
        )
        .await
        .expect("set full day");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert!(task.is_full_day);
    // is_full_day should NOT appear in remaining_data
    assert!(
        !task.remaining_data.contains_key("is_full_day"),
        "dedicated fields excluded from remaining_data"
    );

    // Unset full day
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetIsFullDay { value: false }],
        )
        .await
        .expect("unset full day");

    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert!(!task.is_full_day);
}

#[tokio::test]
async fn test_estimate_round_trip() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Estimate test".into())
        .await
        .expect("create");

    // Default: None
    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.estimate, None);

    // Set estimate = 2 (30 minutes)
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetEstimate { boxes: Some(2) }],
        )
        .await
        .expect("set estimate");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.estimate, Some(2));

    // Clear estimate
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetEstimate { boxes: None }],
        )
        .await
        .expect("clear estimate");

    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert_eq!(task.estimate, None);
}

#[tokio::test]
async fn test_estimate_zero_rejected() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Zero estimate".into())
        .await
        .expect("create");

    let result = session
        .mutate_task(uuid, vec![TaskMutation::SetEstimate { boxes: Some(0) }])
        .await;

    assert!(
        matches!(result, Err(FfiError::InvalidInput { .. })),
        "estimate=0 should be rejected"
    );
}

#[tokio::test]
async fn test_set_value_generic_uda() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Generic UDA".into())
        .await
        .expect("create");

    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetValue {
                key: "custom_field".into(),
                value: Some("hello".into()),
            }],
        )
        .await
        .expect("set generic UDA");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(
        task.remaining_data.get("custom_field").map(String::as_str),
        Some("hello")
    );

    // Clear it
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetValue {
                key: "custom_field".into(),
                value: None,
            }],
        )
        .await
        .expect("clear generic UDA");

    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert!(!task.remaining_data.contains_key("custom_field"));
}

#[tokio::test]
async fn test_set_value_rejects_known_keys() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Known key test".into())
        .await
        .expect("create");

    let result = session
        .mutate_task(
            uuid,
            vec![TaskMutation::SetValue {
                key: "description".into(),
                value: Some("sneaky".into()),
            }],
        )
        .await;

    assert!(
        matches!(result, Err(FfiError::InvalidInput { .. })),
        "known keys should be rejected by SetValue"
    );
}

#[tokio::test]
async fn test_set_value_rejects_flicknote_dedicated_keys() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "FlickNote key guard test".into())
        .await
        .expect("create");

    // is_full_day has a dedicated variant — SetValue must reject it to prevent
    // casing mismatches (e.g. "True" instead of "true") bypassing the typed setter.
    let result = session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetValue {
                key: "is_full_day".into(),
                value: Some("True".into()),
            }],
        )
        .await;
    assert!(
        matches!(result, Err(FfiError::InvalidInput { .. })),
        "is_full_day should be rejected by SetValue"
    );

    // estimate has a dedicated variant with a >0 guard — SetValue must reject it
    // to prevent bypassing that guard via a raw "0" string.
    let result = session
        .mutate_task(
            uuid,
            vec![TaskMutation::SetValue {
                key: "estimate".into(),
                value: Some("0".into()),
            }],
        )
        .await;
    assert!(
        matches!(result, Err(FfiError::InvalidInput { .. })),
        "estimate should be rejected by SetValue"
    );
}

#[tokio::test]
async fn test_recurrence_uda_fields_round_trip() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Recurring template".into())
        .await
        .expect("create");

    // Initially all recurrence fields are None
    let task = session.get_task(uuid.clone()).await.expect("get").unwrap();
    assert_eq!(task.recur, None);
    assert_eq!(task.mask, None);
    assert_eq!(task.imask, None);
    assert_eq!(task.until, None);

    // Set recurrence fields
    let until_epoch: i64 = 1_800_000_000;
    session
        .mutate_task(
            uuid.clone(),
            vec![
                TaskMutation::SetRecur {
                    value: Some("monthly".into()),
                },
                TaskMutation::SetMask {
                    value: Some("-+-".into()),
                },
                TaskMutation::SetImask { value: Some(2) },
                TaskMutation::SetUntil {
                    epoch: Some(until_epoch),
                },
            ],
        )
        .await
        .expect("set recurrence fields");

    let task = session.get_task(uuid.clone()).await.expect("get").unwrap();
    assert_eq!(task.recur, Some("monthly".into()));
    assert_eq!(task.mask, Some("-+-".into()));
    assert_eq!(task.imask, Some(2));
    assert_eq!(task.until, Some(until_epoch));

    // These keys must NOT appear in remaining_data
    assert!(
        !task.remaining_data.contains_key("recur"),
        "recur excluded from remaining_data"
    );
    assert!(
        !task.remaining_data.contains_key("mask"),
        "mask excluded from remaining_data"
    );
    assert!(
        !task.remaining_data.contains_key("imask"),
        "imask excluded from remaining_data"
    );
    assert!(
        !task.remaining_data.contains_key("until"),
        "until excluded from remaining_data"
    );

    // Clear recurrence fields
    session
        .mutate_task(
            uuid.clone(),
            vec![
                TaskMutation::SetRecur { value: None },
                TaskMutation::SetMask { value: None },
                TaskMutation::SetImask { value: None },
                TaskMutation::SetUntil { epoch: None },
            ],
        )
        .await
        .expect("clear recurrence fields");

    let task = session.get_task(uuid).await.expect("get").unwrap();
    assert_eq!(task.recur, None);
    assert_eq!(task.mask, None);
    assert_eq!(task.imask, None);
    assert_eq!(task.until, None);
}

#[tokio::test]
async fn test_set_value_rejects_recurrence_dedicated_keys() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Recurrence key guard test".into())
        .await
        .expect("create");

    for key in &["recur", "mask", "imask", "until"] {
        let result = session
            .mutate_task(
                uuid.clone(),
                vec![TaskMutation::SetValue {
                    key: (*key).into(),
                    value: Some("test".into()),
                }],
            )
            .await;
        assert!(
            matches!(result, Err(FfiError::InvalidInput { .. })),
            "'{key}' should be rejected by SetValue — use the dedicated mutation variant"
        );
    }
}

#[tokio::test]
async fn test_set_project_round_trip() {
    let (session, mock) = make_session_with_executor();
    let uuid = Uuid::new_v4().to_string();

    // Pre-seed project so SetProject can resolve it.
    mock.inject_project("work");

    session
        .create_task(uuid.clone(), "Project test".into())
        .await
        .expect("create");

    // Default: no project.
    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.project, None);
    assert_eq!(task.project_id, None);

    // Set project.
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetProject {
                value: Some("work".into()),
            }],
        )
        .await
        .expect("set project");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.project.as_deref(), Some("work"), "project name set");
    assert!(
        task.project_id.is_some(),
        "project_id should be populated after set"
    );

    // SetProject with nonexistent name should fail with ProjectNotFound.
    let result = session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetProject {
                value: Some("nonexistent".into()),
            }],
        )
        .await;
    assert!(
        matches!(result, Err(FfiError::ProjectNotFound { .. })),
        "expected ProjectNotFound for unknown project"
    );

    // Clear project (None) still works — bypasses resolve_project_id.
    session
        .mutate_task(uuid.clone(), vec![TaskMutation::SetProject { value: None }])
        .await
        .expect("clear project");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.project, None, "project cleared");
    assert_eq!(task.project_id, None, "project_id cleared with project");
}

// ---------------------------------------------------------------------------
// SetProjectId tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_set_project_id_round_trip() {
    let (session, mock) = make_session_with_executor();
    let uuid = Uuid::new_v4().to_string();

    // Pre-seed a project and grab its UUID.
    let project_id = mock.inject_project("inbox");

    session
        .create_task(uuid.clone(), "ProjectId test".into())
        .await
        .expect("create");

    // Set project by UUID.
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetProjectId {
                value: Some(project_id.clone()),
            }],
        )
        .await
        .expect("set project_id");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(
        task.project_id.as_deref(),
        Some(project_id.as_str()),
        "project_id should match injected UUID"
    );
    // project name is resolved via JOIN from project_id → projects table.
    assert_eq!(
        task.project.as_deref(),
        Some("inbox"),
        "project name resolved from JOIN"
    );

    // Clear with None.
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetProjectId { value: None }],
        )
        .await
        .expect("clear project_id");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.project_id, None, "project_id cleared");
}

#[tokio::test]
async fn test_set_project_id_nonexistent_uuid() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "ProjectId nonexistent".into())
        .await
        .expect("create");

    // SetProjectId with a random UUID should succeed (no validation).
    let random_id = Uuid::new_v4().to_string();
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetProjectId {
                value: Some(random_id.clone()),
            }],
        )
        .await
        .expect("set nonexistent project_id should succeed");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(
        task.project_id.as_deref(),
        Some(random_id.as_str()),
        "project_id set to random UUID"
    );
}

#[tokio::test]
async fn test_set_project_id_then_set_project_clears_old_id() {
    let (session, mock) = make_session_with_executor();
    let uuid = Uuid::new_v4().to_string();
    let project_id = mock.inject_project("work");
    mock.inject_project("personal");

    session
        .create_task(uuid.clone(), "Cross-set test".into())
        .await
        .expect("create");

    // Set project by UUID first.
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetProjectId {
                value: Some(project_id.clone()),
            }],
        )
        .await
        .expect("set project_id");

    // Overwrite with SetProject by name.
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetProject {
                value: Some("personal".into()),
            }],
        )
        .await
        .expect("set project by name");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(
        task.project.as_deref(),
        Some("personal"),
        "project name resolved from new name"
    );
    // project_id should now point to "personal", not the old "work" UUID.
    assert_ne!(
        task.project_id.as_deref(),
        Some(project_id.as_str()),
        "project_id should no longer be the old UUID"
    );
}

#[tokio::test]
async fn test_set_project_id_invalid_uuid_rejected() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Invalid UUID test".into())
        .await
        .expect("create");

    let result = session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetProjectId {
                value: Some("not-a-uuid".into()),
            }],
        )
        .await;
    assert!(
        matches!(result, Err(FfiError::InvalidInput { .. })),
        "expected InvalidInput for malformed UUID"
    );
}

// ---------------------------------------------------------------------------
// Reorder tests
// ---------------------------------------------------------------------------

/// Create a task with a position (position must be a valid fractional index string).
async fn create_positioned(session: &FfiSession, desc: &str, pos: &str) -> String {
    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), desc.into())
        .await
        .expect("create");
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetPosition {
                value: Some(pos.into()),
            }],
        )
        .await
        .expect("set position");
    uuid
}

#[tokio::test]
async fn test_reorder_after_middle_sibling() {
    let session = make_session();
    let pos = sequential_positions(3);

    // Three siblings A(pos[0]) < B(pos[1]) < C(pos[2]). Move A after B → B < A < C.
    let a = create_positioned(&session, "A", &pos[0]).await;
    let b = create_positioned(&session, "B", &pos[1]).await;
    let c = create_positioned(&session, "C", &pos[2]).await;

    let task = session
        .reorder_after(a.clone(), b.clone())
        .await
        .expect("reorder_after");

    let new_pos = task.position.as_deref().expect("position set");
    assert!(new_pos > pos[1].as_str(), "A should be after B");
    assert!(new_pos < pos[2].as_str(), "A should be before C");
    let _ = c;
}

#[tokio::test]
async fn test_reorder_after_last_sibling() {
    let session = make_session();
    let pos = sequential_positions(2);

    // Two siblings A(pos[0]) < B(pos[1]). Move A after B → B < A.
    let a = create_positioned(&session, "A", &pos[0]).await;
    let b = create_positioned(&session, "B", &pos[1]).await;

    let task = session
        .reorder_after(a.clone(), b.clone())
        .await
        .expect("reorder_after last");

    let new_pos = task.position.as_deref().expect("position set");
    assert!(new_pos > pos[1].as_str(), "A should be after B");
}

#[tokio::test]
async fn test_reorder_before_middle_sibling() {
    let session = make_session();
    let pos = sequential_positions(3);

    // Three siblings A(pos[0]) < B(pos[1]) < C(pos[2]). Move C before B → A < C < B.
    let a = create_positioned(&session, "A", &pos[0]).await;
    let b = create_positioned(&session, "B", &pos[1]).await;
    let c = create_positioned(&session, "C", &pos[2]).await;

    let task = session
        .reorder_before(c.clone(), b.clone())
        .await
        .expect("reorder_before");

    let new_pos = task.position.as_deref().expect("position set");
    assert!(new_pos > pos[0].as_str(), "C should be after A");
    assert!(new_pos < pos[1].as_str(), "C should be before B");
    let _ = a;
}

#[tokio::test]
async fn test_reorder_before_first_sibling() {
    let session = make_session();
    let pos = sequential_positions(2);

    // Two siblings A(pos[0]) < B(pos[1]). Move B before A → B < A.
    let a = create_positioned(&session, "A", &pos[0]).await;
    let b = create_positioned(&session, "B", &pos[1]).await;

    let task = session
        .reorder_before(b.clone(), a.clone())
        .await
        .expect("reorder_before first");

    let new_pos = task.position.as_deref().expect("position set");
    assert!(new_pos < pos[0].as_str(), "B should be before A");
}

#[tokio::test]
async fn test_reorder_different_parent_rejected() {
    let session = make_session();

    // Two tasks with different parents.
    let parent1 = create_positioned(&session, "Parent1", "10").await;
    let parent2 = create_positioned(&session, "Parent2", "20").await;

    let child1 = Uuid::new_v4().to_string();
    session
        .create_task(child1.clone(), "Child1".into())
        .await
        .unwrap();
    session
        .mutate_task(
            child1.clone(),
            vec![
                TaskMutation::SetParent {
                    uuid: Some(parent1.clone()),
                },
                TaskMutation::SetPosition {
                    value: Some("10".into()),
                },
            ],
        )
        .await
        .unwrap();

    let child2 = Uuid::new_v4().to_string();
    session
        .create_task(child2.clone(), "Child2".into())
        .await
        .unwrap();
    session
        .mutate_task(
            child2.clone(),
            vec![
                TaskMutation::SetParent {
                    uuid: Some(parent2.clone()),
                },
                TaskMutation::SetPosition {
                    value: Some("10".into()),
                },
            ],
        )
        .await
        .unwrap();

    let result = session.reorder_after(child1.clone(), child2.clone()).await;
    assert!(
        matches!(result, Err(FfiError::NotASibling { .. })),
        "expected NotASibling"
    );
    let _ = (parent1, parent2);
}

#[tokio::test]
async fn test_reorder_nonexistent_uuid() {
    let session = make_session();
    let ghost = Uuid::new_v4().to_string();
    let anchor = create_positioned(&session, "Anchor", "10").await;

    let result = session.reorder_after(ghost, anchor).await;
    assert!(
        matches!(result, Err(FfiError::TaskNotFound { .. })),
        "expected TaskNotFound for missing uuid"
    );
}

#[tokio::test]
async fn test_reorder_nonexistent_anchor() {
    let session = make_session();
    let task = create_positioned(&session, "Task", "10").await;
    let ghost_anchor = Uuid::new_v4().to_string();

    let result = session.reorder_after(task, ghost_anchor).await;
    assert!(
        matches!(result, Err(FfiError::TaskNotFound { .. })),
        "expected TaskNotFound for missing anchor"
    );
}

// ---------------------------------------------------------------------------
// Reorder to beginning / end tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_reorder_to_beginning_and_end() {
    let session = make_session();
    let pos = sequential_positions(3);

    let a = create_positioned(&session, "A", &pos[0]).await;
    let b = create_positioned(&session, "B", &pos[1]).await;
    let c = create_positioned(&session, "C", &pos[2]).await;

    // Move A to end → A's position > C's.
    let moved = session.reorder_to_end(a.clone()).await.unwrap();
    let c_task = session.get_task(c.clone()).await.unwrap().unwrap();
    assert!(
        moved.position.as_ref().unwrap() > c_task.position.as_ref().unwrap(),
        "A's position should be after C"
    );

    // Move C to beginning → C's position < B's.
    let moved = session.reorder_to_beginning(c.clone()).await.unwrap();
    let b_task = session.get_task(b.clone()).await.unwrap().unwrap();
    assert!(
        moved.position.as_ref().unwrap() < b_task.position.as_ref().unwrap(),
        "C's position should be before B"
    );
}

#[tokio::test]
async fn test_reorder_to_end_nonexistent() {
    let session = make_session();
    let ghost = Uuid::new_v4().to_string();

    let result = session.reorder_to_end(ghost).await;
    assert!(
        matches!(result, Err(FfiError::TaskNotFound { .. })),
        "expected TaskNotFound"
    );
}

#[tokio::test]
async fn test_reorder_to_beginning_already_first() {
    let session = make_session();
    let pos = sequential_positions(2);

    let a = create_positioned(&session, "A", &pos[0]).await;
    let b = create_positioned(&session, "B", &pos[1]).await;

    // Move A to beginning (already first) — should succeed idempotently.
    let moved = session.reorder_to_beginning(a.clone()).await.unwrap();
    assert!(
        moved.position.is_some(),
        "should have a position after reorder"
    );
    // New position should be less than B's position (A was excluded from
    // siblings during the calculation, so prepend generates before B).
    let b_task = session.get_task(b).await.unwrap().unwrap();
    assert!(
        moved.position.as_ref().unwrap() < b_task.position.as_ref().unwrap(),
        "new position should be less than B's position"
    );
}

#[tokio::test]
async fn test_set_value_rejects_project_keys() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Project key guard".into())
        .await
        .expect("create");

    for key in &["project", "project_id"] {
        let result = session
            .mutate_task(
                uuid.clone(),
                vec![TaskMutation::SetValue {
                    key: (*key).into(),
                    value: Some("test".into()),
                }],
            )
            .await;
        assert!(
            matches!(result, Err(FfiError::InvalidInput { .. })),
            "'{key}' should be rejected by SetValue"
        );
    }
}

// ---------------------------------------------------------------------------
// Reparent and is_ancestor tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_reparent_to_end() {
    let session = make_session();
    let pos = sequential_positions(2);

    let parent = Uuid::new_v4().to_string();
    session
        .create_task(parent.clone(), "Parent".into())
        .await
        .unwrap();

    let child1 = create_positioned(&session, "Child1", &pos[0]).await;
    let child2 = create_positioned(&session, "Child2", &pos[1]).await;
    session
        .mutate_task(
            child1.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent.clone()),
            }],
        )
        .await
        .unwrap();
    session
        .mutate_task(
            child2.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent.clone()),
            }],
        )
        .await
        .unwrap();

    let mover = Uuid::new_v4().to_string();
    session
        .create_task(mover.clone(), "Mover".into())
        .await
        .unwrap();

    let task = session
        .reparent(mover.clone(), Some(parent.clone()), ReparentPosition::End)
        .await
        .expect("reparent to End");
    assert_eq!(task.parent.as_deref(), Some(parent.as_str()));
    let new_pos = task.position.as_deref().expect("position set");
    assert!(new_pos > pos[1].as_str(), "should be after child2");
    let _ = (child1, child2);
}

#[tokio::test]
async fn test_reparent_to_beginning() {
    let session = make_session();
    let pos = sequential_positions(1);

    let parent = Uuid::new_v4().to_string();
    session
        .create_task(parent.clone(), "Parent".into())
        .await
        .unwrap();

    let existing_child = create_positioned(&session, "ExistingChild", &pos[0]).await;
    session
        .mutate_task(
            existing_child.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent.clone()),
            }],
        )
        .await
        .unwrap();

    let mover = Uuid::new_v4().to_string();
    session
        .create_task(mover.clone(), "Mover".into())
        .await
        .unwrap();

    let task = session
        .reparent(
            mover.clone(),
            Some(parent.clone()),
            ReparentPosition::Beginning,
        )
        .await
        .expect("reparent to Beginning");
    assert_eq!(task.parent.as_deref(), Some(parent.as_str()));
    let new_pos = task.position.as_deref().expect("position set");
    assert!(new_pos < pos[0].as_str(), "should be before existing child");
    let _ = existing_child;
}

#[tokio::test]
async fn test_reparent_to_root() {
    let session = make_session();

    let original_parent = Uuid::new_v4().to_string();
    session
        .create_task(original_parent.clone(), "Parent".into())
        .await
        .unwrap();

    let child = Uuid::new_v4().to_string();
    session
        .create_task(child.clone(), "Child".into())
        .await
        .unwrap();
    session
        .mutate_task(
            child.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(original_parent.clone()),
            }],
        )
        .await
        .unwrap();

    let task = session
        .reparent(child.clone(), None, ReparentPosition::End)
        .await
        .expect("reparent to root");
    assert_eq!(task.parent, None, "parent should be cleared");
    let _ = original_parent;
}

#[tokio::test]
async fn test_reparent_circular_rejected() {
    let session = make_session();

    let parent = Uuid::new_v4().to_string();
    session
        .create_task(parent.clone(), "Parent".into())
        .await
        .unwrap();
    let child = Uuid::new_v4().to_string();
    session
        .create_task(child.clone(), "Child".into())
        .await
        .unwrap();
    session
        .mutate_task(
            child.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent.clone()),
            }],
        )
        .await
        .unwrap();

    let result = session
        .reparent(parent.clone(), Some(child.clone()), ReparentPosition::End)
        .await;
    assert!(
        matches!(result, Err(FfiError::CircularParent { .. })),
        "expected CircularParent"
    );
}

#[tokio::test]
async fn test_reparent_with_after_anchor() {
    let session = make_session();
    let pos = sequential_positions(2);

    let parent = Uuid::new_v4().to_string();
    session
        .create_task(parent.clone(), "Parent".into())
        .await
        .unwrap();

    let anchor1 = create_positioned(&session, "Anchor1", &pos[0]).await;
    let anchor2 = create_positioned(&session, "Anchor2", &pos[1]).await;
    session
        .mutate_task(
            anchor1.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent.clone()),
            }],
        )
        .await
        .unwrap();
    session
        .mutate_task(
            anchor2.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent.clone()),
            }],
        )
        .await
        .unwrap();

    let mover = Uuid::new_v4().to_string();
    session
        .create_task(mover.clone(), "Mover".into())
        .await
        .unwrap();

    let task = session
        .reparent(
            mover.clone(),
            Some(parent.clone()),
            ReparentPosition::After {
                anchor: anchor1.clone(),
            },
        )
        .await
        .expect("reparent after anchor");
    let new_pos = task.position.as_deref().expect("position set");
    assert!(new_pos > pos[0].as_str(), "should be after anchor1");
    assert!(new_pos < pos[1].as_str(), "should be before anchor2");
    let _ = anchor2;
}

#[tokio::test]
async fn test_reparent_nonexistent_uuid() {
    let session = make_session();
    let ghost = Uuid::new_v4().to_string();
    let result = session.reparent(ghost, None, ReparentPosition::End).await;
    assert!(
        matches!(result, Err(FfiError::TaskNotFound { .. })),
        "expected TaskNotFound"
    );
}

#[tokio::test]
async fn test_reparent_nonexistent_parent() {
    let session = make_session();
    let task_uuid = Uuid::new_v4().to_string();
    session
        .create_task(task_uuid.clone(), "Task".into())
        .await
        .unwrap();
    let ghost_parent = Uuid::new_v4().to_string();
    let result = session
        .reparent(task_uuid, Some(ghost_parent), ReparentPosition::End)
        .await;
    assert!(
        matches!(result, Err(FfiError::TaskNotFound { .. })),
        "expected TaskNotFound for missing parent"
    );
}

#[tokio::test]
async fn test_is_ancestor_basic() {
    let session = make_session();

    let grandparent = Uuid::new_v4().to_string();
    session
        .create_task(grandparent.clone(), "Grandparent".into())
        .await
        .unwrap();
    let parent = Uuid::new_v4().to_string();
    session
        .create_task(parent.clone(), "Parent".into())
        .await
        .unwrap();
    session
        .mutate_task(
            parent.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(grandparent.clone()),
            }],
        )
        .await
        .unwrap();
    let child = Uuid::new_v4().to_string();
    session
        .create_task(child.clone(), "Child".into())
        .await
        .unwrap();
    session
        .mutate_task(
            child.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent.clone()),
            }],
        )
        .await
        .unwrap();

    assert!(
        session
            .is_ancestor(child.clone(), grandparent.clone())
            .await
            .unwrap(),
        "grandparent is ancestor of child"
    );
    assert!(
        !session
            .is_ancestor(grandparent.clone(), child.clone())
            .await
            .unwrap(),
        "child is NOT ancestor of grandparent"
    );

    let unrelated = Uuid::new_v4().to_string();
    session
        .create_task(unrelated.clone(), "Unrelated".into())
        .await
        .unwrap();
    assert!(
        !session
            .is_ancestor(child.clone(), unrelated.clone())
            .await
            .unwrap(),
        "unrelated is not ancestor of child"
    );
    let _ = (grandparent, parent, unrelated);
}

// ---------------------------------------------------------------------------
// Test gap #11: SetStatus to non-pending auto-clears xstatus
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_xstatus_auto_clears_on_setstatus_completed() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"xstatus":[{"name":"blocked","icon":128721}]}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "SetStatus test".into())
        .await
        .unwrap();
    session
        .set_xstatus(uuid.clone(), "blocked".into())
        .await
        .unwrap();

    // SetStatus { Completed } exercises a different match arm than Done / Delete.
    let task = session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetStatus {
                status: FfiStatus::Completed,
            }],
        )
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        task.xstatus, None,
        "xstatus must clear when SetStatus → Completed"
    );
    assert!(matches!(task.status, FfiStatus::Completed));
}

// ---------------------------------------------------------------------------
// Test gap #12: clear_xstatus when xstatus is already None
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_clear_xstatus_when_already_none() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "No xstatus".into())
        .await
        .unwrap();

    // clear_xstatus on a task that has no xstatus should succeed and return
    // the task unchanged (no undo point emitted, status stays Pending).
    let task = session.clear_xstatus(uuid.clone()).await.unwrap();
    assert_eq!(task.xstatus, None, "xstatus still None");
    assert!(
        matches!(task.status, FfiStatus::Pending),
        "status unchanged"
    );
}

// ---------------------------------------------------------------------------
// Test gap #13: reparent Before anchor + anchor_idx == 0 boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_reparent_with_before_anchor() {
    let session = make_session();
    let pos = sequential_positions(2);

    let parent = Uuid::new_v4().to_string();
    session
        .create_task(parent.clone(), "Parent".into())
        .await
        .unwrap();

    let anchor1 = create_positioned(&session, "Anchor1", &pos[0]).await;
    let anchor2 = create_positioned(&session, "Anchor2", &pos[1]).await;
    session
        .mutate_task(
            anchor1.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent.clone()),
            }],
        )
        .await
        .unwrap();
    session
        .mutate_task(
            anchor2.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent.clone()),
            }],
        )
        .await
        .unwrap();

    let mover = Uuid::new_v4().to_string();
    session
        .create_task(mover.clone(), "Mover".into())
        .await
        .unwrap();

    // Insert mover before anchor2 (middle of list) → pos[0] < mover < pos[1].
    let task = session
        .reparent(
            mover.clone(),
            Some(parent.clone()),
            ReparentPosition::Before {
                anchor: anchor2.clone(),
            },
        )
        .await
        .expect("reparent before anchor");
    let new_pos = task.position.as_deref().expect("position set");
    assert!(new_pos > pos[0].as_str(), "should be after anchor1");
    assert!(new_pos < pos[1].as_str(), "should be before anchor2");
    let _ = (anchor1, anchor2);
}

#[tokio::test]
async fn test_reparent_before_first_child_prepends() {
    let session = make_session();
    let pos = sequential_positions(1);

    let parent = Uuid::new_v4().to_string();
    session
        .create_task(parent.clone(), "Parent".into())
        .await
        .unwrap();

    // anchor_idx == 0 path — exercises prepend_position.
    let first_child = create_positioned(&session, "FirstChild", &pos[0]).await;
    session
        .mutate_task(
            first_child.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent.clone()),
            }],
        )
        .await
        .unwrap();

    let mover = Uuid::new_v4().to_string();
    session
        .create_task(mover.clone(), "Mover".into())
        .await
        .unwrap();

    let task = session
        .reparent(
            mover.clone(),
            Some(parent.clone()),
            ReparentPosition::Before {
                anchor: first_child.clone(),
            },
        )
        .await
        .expect("reparent before first child");
    let new_pos = task.position.as_deref().expect("position set");
    assert!(new_pos < pos[0].as_str(), "should be before first_child");
    let _ = first_child;
}

// ---------------------------------------------------------------------------
// Test gap #14: deep-chain cycle detection (3-level)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_reparent_deep_chain_cycle_rejected() {
    let session = make_session();

    // grandparent → parent → child chain.
    let grandparent = Uuid::new_v4().to_string();
    session
        .create_task(grandparent.clone(), "Grandparent".into())
        .await
        .unwrap();
    let parent = Uuid::new_v4().to_string();
    session
        .create_task(parent.clone(), "Parent".into())
        .await
        .unwrap();
    session
        .mutate_task(
            parent.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(grandparent.clone()),
            }],
        )
        .await
        .unwrap();
    let child = Uuid::new_v4().to_string();
    session
        .create_task(child.clone(), "Child".into())
        .await
        .unwrap();
    session
        .mutate_task(
            child.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent.clone()),
            }],
        )
        .await
        .unwrap();

    // Reparenting grandparent under child would create a 3-level cycle.
    let result = session
        .reparent(
            grandparent.clone(),
            Some(child.clone()),
            ReparentPosition::End,
        )
        .await;
    assert!(
        matches!(result, Err(FfiError::CircularParent { .. })),
        "expected CircularParent for deep chain cycle"
    );
    let _ = (parent, child);
}

// ---------------------------------------------------------------------------
// Test gap #16: malformed tc_settings JSON returns an error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_malformed_tc_settings_propagates_error() {
    let (session, mock) = make_session_with_executor();
    // Inject invalid JSON — not a valid TcConfig.
    mock.inject_tc_config(r#"not valid json {"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Malformed config test".into())
        .await
        .unwrap();

    // Operations that load tc_config should surface the parse error.
    let result = session.set_xstatus(uuid.clone(), "blocked".into()).await;
    assert!(
        result.is_err(),
        "malformed tc_settings JSON must return an error"
    );
}

// ---------------------------------------------------------------------------
// Test gap #4: AnchorHasNoPosition error
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// create_tag tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_tag_success() {
    let (session, mock) = make_session_with_executor();
    // Start with no tags.
    session.create_tag("work".into()).await.expect("create_tag");
    // Config should now contain "work".
    let config = mock.read_tc_config();
    assert!(
        config.contains("work"),
        "config should contain 'work': {config}"
    );
}

#[tokio::test]
async fn test_create_tag_already_exists() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"tags":"work"}"#);

    let result = session.create_tag("work".into()).await;
    assert!(
        matches!(result, Err(FfiError::TagAlreadyExists { .. })),
        "expected TagAlreadyExists, got: {result:?}"
    );
}

#[tokio::test]
async fn test_create_tag_invalid_name() {
    let session = make_session();
    // Tag names cannot start with digits.
    let result = session.create_tag("123bad".into()).await;
    assert!(
        matches!(result, Err(FfiError::InvalidInput { .. })),
        "expected InvalidInput, got: {result:?}"
    );
}

#[tokio::test]
async fn test_create_tag_rejects_synthetic() {
    let session = make_session();
    // "WAITING" is a synthetic tag — cannot be registered.
    let result = session.create_tag("WAITING".into()).await;
    assert!(
        matches!(result, Err(FfiError::InvalidInput { .. })),
        "expected InvalidInput for synthetic tag, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// mutate_task AddTag pre-validation tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_mutate_task_add_tag_rejected_when_not_in_config() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Tag test".into())
        .await
        .expect("create");

    // "work" is not in tc_config — must return TagNotFound.
    let result = session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::AddTag { tag: "work".into() }],
        )
        .await;
    assert!(
        matches!(result, Err(FfiError::TagNotFound { .. })),
        "expected TagNotFound for unregistered tag"
    );

    // The task must be unchanged (no partial mutation committed).
    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert!(
        task.tags.is_empty(),
        "no tags should be on task after rejection"
    );
}

#[tokio::test]
async fn test_mutate_task_add_tag_invalid_name_returns_invalid_input() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Invalid tag name test".into())
        .await
        .expect("create");

    let result = session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::AddTag { tag: "!bad".into() }],
        )
        .await;
    assert!(
        matches!(result, Err(FfiError::InvalidInput { .. })),
        "expected InvalidInput for bad tag name"
    );
}

#[tokio::test]
async fn test_mutate_task_add_tag_batch_atomicity() {
    // A batch with a valid mutation followed by an unregistered AddTag must
    // commit nothing — the description change must not be persisted.
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "Original".into())
        .await
        .expect("create");

    let result = session
        .mutate_task(
            uuid.clone(),
            vec![
                TaskMutation::SetDescription {
                    value: "Changed".into(),
                },
                TaskMutation::AddTag {
                    tag: "unregistered".into(),
                },
            ],
        )
        .await;
    assert!(
        matches!(result, Err(FfiError::TagNotFound { .. })),
        "expected TagNotFound for unregistered tag in batch"
    );

    // Description must be unchanged — pre-validation aborts before any ops are built.
    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert_eq!(
        task.description, "Original",
        "description must not change on pre-validation failure"
    );
}

#[tokio::test]
async fn test_reorder_anchor_with_no_position_returns_error() {
    let session = make_session();
    let pos = sequential_positions(1);

    // task has a position; anchor exists in DB but has no position field.
    let task_uuid = create_positioned(&session, "Task", &pos[0]).await;
    let unpositioned = Uuid::new_v4().to_string();
    session
        .create_task(unpositioned.clone(), "No position".into())
        .await
        .unwrap();

    let result = session
        .reorder_after(task_uuid.clone(), unpositioned.clone())
        .await;
    assert!(
        matches!(result, Err(FfiError::AnchorHasNoPosition { .. })),
        "expected AnchorHasNoPosition"
    );
}
