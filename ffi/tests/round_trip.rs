//! End-to-end round-trip test exercising the FFI surface via a
//! MockFfiSqlExecutor backed by in-memory SQLite.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use taskchampion_ffi::{
    replica_ops::FfiSession,
    types::{
        FfiError, FfiSqlExecutor, FfiSqlParam, FfiSqlRow, FfiSqlStatement, FfiSqlValue, FfiStatus,
        TaskMutation,
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
            CREATE TABLE IF NOT EXISTS tc_tag_colors (
                id TEXT PRIMARY KEY, name TEXT NOT NULL,
                color TEXT NOT NULL,
                created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
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
                ValueRef::Blob(_) => FfiSqlValue::Null,
            };
            values.push(val);
        }
        Ok(FfiSqlRow { columns, values })
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
    let session = make_session();
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
async fn test_get_all_tags() {
    let session = make_session();

    // Empty — no tasks yet.
    let tags = session.get_all_tags().await.unwrap();
    assert!(tags.is_empty());

    // Create two tasks with overlapping tags.
    let uuid1 = Uuid::new_v4().to_string();
    session
        .create_task(uuid1.clone(), "Task 1".into())
        .await
        .unwrap();
    session
        .mutate_task(
            uuid1.clone(),
            vec![
                TaskMutation::AddTag { tag: "work".into() },
                TaskMutation::AddTag {
                    tag: "urgent".into(),
                },
            ],
        )
        .await
        .unwrap();

    let uuid2 = Uuid::new_v4().to_string();
    session
        .create_task(uuid2.clone(), "Task 2".into())
        .await
        .unwrap();
    session
        .mutate_task(
            uuid2.clone(),
            vec![
                TaskMutation::AddTag { tag: "work".into() },
                TaskMutation::AddTag { tag: "home".into() },
            ],
        )
        .await
        .unwrap();

    let tags = session.get_all_tags().await.unwrap();
    assert_eq!(tags, vec!["home", "urgent", "work"]);
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

    let task = session
        .get_task(uuid)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(
        task.position.as_deref(),
        Some("80"),
        "numeric position string must survive round-trip"
    );
}

#[tokio::test]
async fn test_get_tag_color_returns_empty_string() {
    let session = make_session();

    // No color set — FFI returns empty string.
    let color = session.get_tag_color("work".into()).await.unwrap();
    assert_eq!(color, "");

    // Set and read back.
    session
        .set_tag_color("work".into(), "#ff0000".into())
        .await
        .unwrap();
    let color = session.get_tag_color("work".into()).await.unwrap();
    assert_eq!(color, "#ff0000");
}
