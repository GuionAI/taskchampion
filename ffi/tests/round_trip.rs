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
                wait_at TEXT, parent_id TEXT, position TEXT, project_id TEXT,
                note_id TEXT
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
            CREATE TABLE IF NOT EXISTS settings (
                id TEXT PRIMARY KEY,
                tc_config TEXT NOT NULL DEFAULT '{}'
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
            "INSERT OR REPLACE INTO settings (id, tc_config) VALUES ('tc_config', ?)",
            rusqlite::params![json],
        )
        .expect("inject_tc_config");
    }

    /// Read the current tc_config JSON value from tc_settings.
    fn read_tc_config(&self) -> String {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT tc_config FROM settings WHERE id = 'tc_config'",
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
    mock.inject_tc_config(r#"{"tags":["work"]}"#);
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
async fn test_complete_tree_completes_pending_descendants_in_one_undo_group() {
    let session = make_session();
    let parent_uuid = Uuid::new_v4().to_string();
    let child_uuid = Uuid::new_v4().to_string();
    let grandchild_uuid = Uuid::new_v4().to_string();
    let already_done_uuid = Uuid::new_v4().to_string();
    let dependent_uuid = Uuid::new_v4().to_string();

    session
        .create_task(parent_uuid.clone(), "Parent".into())
        .await
        .expect("create parent");
    session
        .create_task(child_uuid.clone(), "Child".into())
        .await
        .expect("create child");
    session
        .create_task(grandchild_uuid.clone(), "Grandchild".into())
        .await
        .expect("create grandchild");
    session
        .create_task(already_done_uuid.clone(), "Already done".into())
        .await
        .expect("create done child");
    session
        .create_task(dependent_uuid.clone(), "Depends on parent".into())
        .await
        .expect("create dependent");

    session
        .mutate_task(
            child_uuid.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent_uuid.clone()),
            }],
        )
        .await
        .expect("set child parent");
    session
        .mutate_task(
            grandchild_uuid.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(child_uuid.clone()),
            }],
        )
        .await
        .expect("set grandchild parent");
    session
        .mutate_task(
            already_done_uuid.clone(),
            vec![
                TaskMutation::SetParent {
                    uuid: Some(parent_uuid.clone()),
                },
                TaskMutation::Done,
            ],
        )
        .await
        .expect("complete child upfront");
    session
        .mutate_task(
            dependent_uuid.clone(),
            vec![TaskMutation::AddDependency {
                uuid: parent_uuid.clone(),
            }],
        )
        .await
        .expect("add dependency");

    let completed = session
        .complete_tree(parent_uuid.clone(), None)
        .await
        .expect("complete tree");
    let completed_uuids: Vec<_> = completed.iter().map(|t| t.uuid.as_str()).collect();
    assert_eq!(completed_uuids.len(), 3);
    assert!(completed_uuids.contains(&parent_uuid.as_str()));
    assert!(completed_uuids.contains(&child_uuid.as_str()));
    assert!(completed_uuids.contains(&grandchild_uuid.as_str()));
    assert!(!completed_uuids.contains(&already_done_uuid.as_str()));
    let completed_parent = completed
        .iter()
        .find(|t| t.uuid == parent_uuid)
        .expect("parent returned");
    assert!(
        !completed_parent.is_blocking,
        "completed parent should not be returned with stale dependency state"
    );

    for uuid in [
        &parent_uuid,
        &child_uuid,
        &grandchild_uuid,
        &already_done_uuid,
    ] {
        let task = session
            .get_task(uuid.clone())
            .await
            .expect("get task")
            .expect("task exists");
        assert!(matches!(task.status, FfiStatus::Completed));
    }

    let undone = session.undo().await.expect("undo complete_tree");
    assert!(undone);

    for uuid in [&parent_uuid, &child_uuid, &grandchild_uuid] {
        let task = session
            .get_task(uuid.clone())
            .await
            .expect("get task after undo")
            .expect("task exists after undo");
        assert!(matches!(task.status, FfiStatus::Pending));
        assert_eq!(task.end, None);
    }

    let done_child = session
        .get_task(already_done_uuid.clone())
        .await
        .expect("get done child after undo")
        .expect("done child exists after undo");
    assert!(matches!(done_child.status, FfiStatus::Completed));
}

#[tokio::test]
async fn test_complete_tree_rejects_non_pending_parent() {
    let session = make_session();
    let parent_uuid = Uuid::new_v4().to_string();

    session
        .create_task(parent_uuid.clone(), "Parent".into())
        .await
        .expect("create parent");
    session
        .mutate_task(parent_uuid.clone(), vec![TaskMutation::Done])
        .await
        .expect("complete parent");

    let result = session.complete_tree(parent_uuid.clone(), None).await;
    assert!(
        matches!(result, Err(FfiError::InvalidInput { .. })),
        "expected InvalidInput for non-pending parent"
    );
}

#[tokio::test]
async fn test_complete_tree_dry_run_does_not_mutate() {
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
        .expect("set child parent");

    let preview = session
        .complete_tree(parent_uuid.clone(), Some(true))
        .await
        .expect("complete tree dry run");
    let preview_uuids: Vec<_> = preview.iter().map(|t| t.uuid.as_str()).collect();
    assert_eq!(preview_uuids.len(), 2);
    assert!(preview_uuids.contains(&parent_uuid.as_str()));
    assert!(preview_uuids.contains(&child_uuid.as_str()));

    for uuid in [&parent_uuid, &child_uuid] {
        let task = session
            .get_task(uuid.clone())
            .await
            .expect("get task after dry run")
            .expect("task exists after dry run");
        assert!(matches!(task.status, FfiStatus::Pending));
        assert_eq!(task.end, None);
    }
}

#[tokio::test]
async fn test_delete_tree_deletes_descendants_in_one_undo_group() {
    let session = make_session();
    let parent_uuid = Uuid::new_v4().to_string();
    let child_uuid = Uuid::new_v4().to_string();
    let grandchild_uuid = Uuid::new_v4().to_string();
    let already_deleted_uuid = Uuid::new_v4().to_string();

    session
        .create_task(parent_uuid.clone(), "Parent".into())
        .await
        .expect("create parent");
    session
        .create_task(child_uuid.clone(), "Child".into())
        .await
        .expect("create child");
    session
        .create_task(grandchild_uuid.clone(), "Grandchild".into())
        .await
        .expect("create grandchild");
    session
        .create_task(already_deleted_uuid.clone(), "Already deleted".into())
        .await
        .expect("create deleted child");

    session
        .mutate_task(
            child_uuid.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(parent_uuid.clone()),
            }],
        )
        .await
        .expect("set child parent");
    session
        .mutate_task(
            grandchild_uuid.clone(),
            vec![TaskMutation::SetParent {
                uuid: Some(child_uuid.clone()),
            }],
        )
        .await
        .expect("set grandchild parent");
    session
        .mutate_task(
            already_deleted_uuid.clone(),
            vec![
                TaskMutation::SetParent {
                    uuid: Some(parent_uuid.clone()),
                },
                TaskMutation::Delete,
            ],
        )
        .await
        .expect("delete child upfront");

    let deleted = session
        .delete_tree(parent_uuid.clone(), None)
        .await
        .expect("delete tree");
    let deleted_uuids: Vec<_> = deleted.iter().map(|t| t.uuid.as_str()).collect();
    assert_eq!(deleted_uuids.len(), 3);
    assert!(deleted_uuids.contains(&parent_uuid.as_str()));
    assert!(deleted_uuids.contains(&child_uuid.as_str()));
    assert!(deleted_uuids.contains(&grandchild_uuid.as_str()));
    assert!(!deleted_uuids.contains(&already_deleted_uuid.as_str()));

    for uuid in [
        &parent_uuid,
        &child_uuid,
        &grandchild_uuid,
        &already_deleted_uuid,
    ] {
        let task = session
            .get_task(uuid.clone())
            .await
            .expect("get task")
            .expect("task exists");
        assert!(matches!(task.status, FfiStatus::Deleted));
    }

    let undone = session.undo().await.expect("undo delete_tree");
    assert!(undone);

    for uuid in [&parent_uuid, &child_uuid, &grandchild_uuid] {
        let task = session
            .get_task(uuid.clone())
            .await
            .expect("get task after undo")
            .expect("task exists after undo");
        assert!(matches!(task.status, FfiStatus::Pending));
        assert_eq!(task.end, None);
    }

    let deleted_child = session
        .get_task(already_deleted_uuid.clone())
        .await
        .expect("get deleted child after undo")
        .expect("deleted child exists after undo");
    assert!(matches!(deleted_child.status, FfiStatus::Deleted));
}

#[tokio::test]
async fn test_delete_tree_dry_run_does_not_mutate() {
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
        .expect("set child parent");

    let preview = session
        .delete_tree(parent_uuid.clone(), Some(true))
        .await
        .expect("delete tree dry run");
    let preview_uuids: Vec<_> = preview.iter().map(|t| t.uuid.as_str()).collect();
    assert_eq!(preview_uuids.len(), 2);
    assert!(preview_uuids.contains(&parent_uuid.as_str()));
    assert!(preview_uuids.contains(&child_uuid.as_str()));

    for uuid in [&parent_uuid, &child_uuid] {
        let task = session
            .get_task(uuid.clone())
            .await
            .expect("get task after dry run")
            .expect("task exists after dry run");
        assert!(matches!(task.status, FfiStatus::Pending));
        assert_eq!(task.end, None);
    }
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
    mock.inject_tc_config(r#"{"tags":["work","home"]}"#);

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
    mock.inject_tc_config(r#"{"tags":["work"]}"#);

    let result = session.delete_tag("ghost".into()).await;
    assert!(
        matches!(result, Err(FfiError::TagNotFound { .. })),
        "expected TagNotFound, got: {result:?}"
    );
}

#[tokio::test]
async fn test_rename_tag_success() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"tags":["oldtag","home"]}"#);

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
    mock.inject_tc_config(r#"{"tags":["work"]}"#);

    let result = session.rename_tag("ghost".into(), "other".into()).await;
    assert!(
        matches!(result, Err(FfiError::TagNotFound { .. })),
        "expected TagNotFound, got: {result:?}"
    );
}

#[tokio::test]
async fn test_rename_tag_already_exists() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"tags":["old","new"]}"#);

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
async fn test_end_epoch_round_trip() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();
    let epoch: i64 = 1_700_000_000;

    session
        .create_task(uuid.clone(), "End test".into())
        .await
        .expect("create");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.end, None);

    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetEnd { epoch: Some(epoch) }],
        )
        .await
        .expect("set end");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.end, Some(epoch));

    session
        .mutate_task(uuid.clone(), vec![TaskMutation::SetEnd { epoch: None }])
        .await
        .expect("clear end");

    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert_eq!(task.end, None);
}

#[tokio::test]
async fn test_priority_clear_round_trip() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Priority test".into())
        .await
        .expect("create");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.priority, None, "unset priority should read as None");

    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetPriority {
                value: Some("H".into()),
            }],
        )
        .await
        .expect("set priority");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.priority, Some("H".into()));

    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetPriority { value: None }],
        )
        .await
        .expect("clear priority");

    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert_eq!(task.priority, None, "cleared priority should read as None");
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

#[tokio::test]
async fn test_set_note_id_round_trip() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();
    let note_uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "SetNoteId test".into())
        .await
        .expect("create");

    // Set note_id.
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetNoteId {
                value: Some(note_uuid.clone()),
            }],
        )
        .await
        .expect("set note_id");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(
        task.note_id.as_deref(),
        Some(note_uuid.as_str()),
        "note_id should match"
    );

    // Clear note_id.
    session
        .mutate_task(uuid.clone(), vec![TaskMutation::SetNoteId { value: None }])
        .await
        .expect("clear note_id");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(task.note_id.as_deref(), None, "note_id should be cleared");
}

#[tokio::test]
async fn test_set_note_id_nonexistent_uuid_accepted() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();
    let random_id = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "NoteId nonexistent".into())
        .await
        .expect("create");

    // SetNoteId with a random UUID should succeed (no existence validation).
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetNoteId {
                value: Some(random_id.clone()),
            }],
        )
        .await
        .expect("set nonexistent note_id should succeed");

    let task = session.get_task(uuid.clone()).await.unwrap().unwrap();
    assert_eq!(
        task.note_id.as_deref(),
        Some(random_id.as_str()),
        "note_id set to random UUID"
    );
}

#[tokio::test]
async fn test_set_note_id_invalid_uuid_rejected() {
    let session = make_session();
    let uuid = Uuid::new_v4().to_string();

    session
        .create_task(uuid.clone(), "Invalid note UUID test".into())
        .await
        .expect("create");

    let result = session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetNoteId {
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
// Test: SetStatus to Pending also clears xstatus (the bug fix)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_xstatus_auto_clears_on_setstatus_pending() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"xstatus":[{"name":"blocked","icon":128721}]}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "SetStatus to Pending test".into())
        .await
        .unwrap();
    session
        .set_xstatus(uuid.clone(), "blocked".into())
        .await
        .unwrap();

    // SetStatus { Pending } directly on a Pending task that already has xstatus
    // set. The old buggy code skipped clear_xstatus_if_set when new_status ==
    // Pending, leaving xstatus intact. The fix removes that guard.
    let task = session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetStatus {
                status: FfiStatus::Pending,
            }],
        )
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        task.xstatus, None,
        "xstatus must clear when SetStatus to Pending"
    );
    assert!(matches!(task.status, FfiStatus::Pending));
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
    mock.inject_tc_config("{}");
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
    mock.inject_tc_config(r#"{"tags":["work"]}"#);

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

// ── xstatus lifecycle ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_create_xstatus_success() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config("{}");
    session
        .create_xstatus("blocked".into(), 128721)
        .await
        .unwrap();
    let config: serde_json::Value = serde_json::from_str(&mock.read_tc_config()).unwrap();
    let xs = config["xstatus"].as_array().unwrap();
    assert_eq!(xs.len(), 1);
    assert_eq!(xs[0]["name"], "blocked");
    assert_eq!(xs[0]["icon"], 128721);
}

#[tokio::test]
async fn test_create_xstatus_already_exists() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"xstatus":[{"name":"blocked","icon":128721}]}"#);
    let result = session.create_xstatus("blocked".into(), 9999).await;
    assert!(
        matches!(result, Err(FfiError::XStatusAlreadyExists { .. })),
        "expected XStatusAlreadyExists, got: {result:?}"
    );
}

#[tokio::test]
async fn test_delete_xstatus_removes_from_config_and_tasks() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"xstatus":[{"name":"blocked","icon":128721}]}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "XStatus delete test".into())
        .await
        .unwrap();
    session
        .set_xstatus(uuid.clone(), "blocked".into())
        .await
        .unwrap();

    let count = session.delete_xstatus("blocked".into()).await.unwrap();
    assert_eq!(count, 1, "one task should have xstatus cleared");

    // Config no longer has 'blocked'.
    let config: serde_json::Value = serde_json::from_str(&mock.read_tc_config()).unwrap();
    let xs = config["xstatus"].as_array().unwrap();
    assert!(xs.is_empty());

    // Task no longer has xstatus.
    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert_eq!(task.xstatus, None);
}

#[tokio::test]
async fn test_delete_xstatus_not_found() {
    let (session, _mock) = make_session_with_executor();
    let result = session.delete_xstatus("ghost".into()).await;
    assert!(
        matches!(result, Err(FfiError::XStatusNotFound { .. })),
        "expected XStatusNotFound, got: {result:?}"
    );
}

#[tokio::test]
async fn test_rename_xstatus_success() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(r#"{"xstatus":[{"name":"blocked","icon":128721}]}"#);

    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), "XStatus rename test".into())
        .await
        .unwrap();
    session
        .set_xstatus(uuid.clone(), "blocked".into())
        .await
        .unwrap();

    let count = session
        .rename_xstatus("blocked".into(), "waiting".into())
        .await
        .unwrap();
    assert_eq!(count, 1, "one task should have xstatus renamed");

    // Config updated.
    let config: serde_json::Value = serde_json::from_str(&mock.read_tc_config()).unwrap();
    let xs = config["xstatus"].as_array().unwrap();
    assert_eq!(xs.len(), 1);
    assert_eq!(xs[0]["name"], "waiting");
    assert_eq!(xs[0]["icon"], 128721);

    // Task updated.
    let task = session.get_task(uuid).await.unwrap().unwrap();
    assert_eq!(task.xstatus.as_deref(), Some("waiting"));
}

#[tokio::test]
async fn test_rename_xstatus_not_found() {
    let (session, _mock) = make_session_with_executor();
    let result = session.rename_xstatus("ghost".into(), "new".into()).await;
    assert!(
        matches!(result, Err(FfiError::XStatusNotFound { .. })),
        "expected XStatusNotFound, got: {result:?}"
    );
}

#[tokio::test]
async fn test_rename_xstatus_already_exists() {
    let (session, mock) = make_session_with_executor();
    mock.inject_tc_config(
        r#"{"xstatus":[{"name":"blocked","icon":1},{"name":"waiting","icon":2}]}"#,
    );
    let result = session
        .rename_xstatus("blocked".into(), "waiting".into())
        .await;
    assert!(
        matches!(result, Err(FfiError::XStatusAlreadyExists { .. })),
        "expected XStatusAlreadyExists, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Today-view reorder tests
// ---------------------------------------------------------------------------

async fn create_today_positioned(session: &FfiSession, desc: &str, pos: &str) -> String {
    let uuid = Uuid::new_v4().to_string();
    session
        .create_task(uuid.clone(), desc.into())
        .await
        .expect("create");
    session
        .mutate_task(
            uuid.clone(),
            vec![TaskMutation::SetTodayPosition {
                value: Some(pos.into()),
            }],
        )
        .await
        .expect("set today_position");
    uuid
}

#[tokio::test]
async fn test_today_reorder_after_middle() {
    let session = make_session();
    let pos = sequential_positions(3);

    // A(pos[0]) < B(pos[1]) < C(pos[2]). Move A after B → B < A < C.
    let a = create_today_positioned(&session, "A", &pos[0]).await;
    let b = create_today_positioned(&session, "B", &pos[1]).await;
    let _c = create_today_positioned(&session, "C", &pos[2]).await;

    let task = session
        .today_reorder_after(a.clone(), b.clone())
        .await
        .expect("today_reorder_after");

    let new_pos = task.today_position.as_deref().expect("today_position set");
    assert!(new_pos > pos[1].as_str(), "A should be after B");
    assert!(new_pos < pos[2].as_str(), "A should be before C");
}

#[tokio::test]
async fn test_today_reorder_after_last() {
    let session = make_session();
    let pos = sequential_positions(2);

    let a = create_today_positioned(&session, "A", &pos[0]).await;
    let b = create_today_positioned(&session, "B", &pos[1]).await;

    let task = session
        .today_reorder_after(a.clone(), b.clone())
        .await
        .expect("today_reorder_after last");

    let new_pos = task.today_position.as_deref().expect("today_position set");
    assert!(new_pos > pos[1].as_str(), "A should be after B");
}

#[tokio::test]
async fn test_today_reorder_before_middle() {
    let session = make_session();
    let pos = sequential_positions(3);

    // A(pos[0]) < B(pos[1]) < C(pos[2]). Move C before B → A < C < B.
    let _a = create_today_positioned(&session, "A", &pos[0]).await;
    let b = create_today_positioned(&session, "B", &pos[1]).await;
    let c = create_today_positioned(&session, "C", &pos[2]).await;

    let task = session
        .today_reorder_before(c.clone(), b.clone())
        .await
        .expect("today_reorder_before");

    let new_pos = task.today_position.as_deref().expect("today_position set");
    assert!(new_pos > pos[0].as_str(), "C should be after A");
    assert!(new_pos < pos[1].as_str(), "C should be before B");
}

#[tokio::test]
async fn test_today_reorder_before_first() {
    let session = make_session();
    let pos = sequential_positions(2);

    let a = create_today_positioned(&session, "A", &pos[0]).await;
    let b = create_today_positioned(&session, "B", &pos[1]).await;

    let task = session
        .today_reorder_before(b.clone(), a.clone())
        .await
        .expect("today_reorder_before first");

    let new_pos = task.today_position.as_deref().expect("today_position set");
    assert!(new_pos < pos[0].as_str(), "B should be before A");
}

#[tokio::test]
async fn test_today_reorder_to_beginning_and_end() {
    let session = make_session();
    let pos = sequential_positions(3);

    let a = create_today_positioned(&session, "A", &pos[0]).await;
    let b = create_today_positioned(&session, "B", &pos[1]).await;
    let c = create_today_positioned(&session, "C", &pos[2]).await;

    // Move A to end → B < C < A.
    let moved = session.today_reorder_to_end(a.clone()).await.unwrap();
    let a_pos = moved.today_position.as_deref().expect("today_position");
    assert!(a_pos > pos[2].as_str(), "A should be after C");

    // Move C to beginning → C < B < A.
    // Re-fetch B from DB to get its stable position (unchanged by previous move).
    let b_task = session.get_task(b.clone()).await.unwrap().unwrap();
    let b_pos = b_task.today_position.as_deref().expect("B today_position");
    let moved = session.today_reorder_to_beginning(c.clone()).await.unwrap();
    let c_pos = moved.today_position.as_deref().expect("today_position");
    assert!(c_pos < b_pos, "C should be before B");
}

#[tokio::test]
async fn test_today_reorder_anchor_no_today_position() {
    let session = make_session();
    let pos = sequential_positions(1);

    let task = create_today_positioned(&session, "Task", &pos[0]).await;
    // Anchor has no today_position.
    let unpositioned = Uuid::new_v4().to_string();
    session
        .create_task(unpositioned.clone(), "Unpositioned".into())
        .await
        .expect("create");

    let result = session
        .today_reorder_after(task.clone(), unpositioned.clone())
        .await;
    assert!(
        matches!(result, Err(FfiError::AnchorHasNoPosition { .. })),
        "expected AnchorHasNoPosition"
    );
}

#[tokio::test]
async fn test_today_reorder_nonexistent_uuid() {
    let session = make_session();
    let pos = sequential_positions(1);
    let anchor = create_today_positioned(&session, "Anchor", &pos[0]).await;
    let ghost = Uuid::new_v4().to_string();

    let result = session.today_reorder_after(ghost, anchor).await;
    assert!(
        matches!(result, Err(FfiError::TaskNotFound { .. })),
        "expected TaskNotFound"
    );
}

#[tokio::test]
async fn test_today_reorder_nonexistent_anchor() {
    let session = make_session();
    let pos = sequential_positions(1);
    let task = create_today_positioned(&session, "Task", &pos[0]).await;
    let ghost = Uuid::new_v4().to_string();

    let result = session.today_reorder_after(task, ghost).await;
    assert!(
        matches!(result, Err(FfiError::TaskNotFound { .. })),
        "expected TaskNotFound"
    );
}

#[tokio::test]
async fn test_today_reorder_before_nonexistent_uuid() {
    let session = make_session();
    let pos = sequential_positions(1);
    let anchor = create_today_positioned(&session, "Anchor", &pos[0]).await;
    let ghost = Uuid::new_v4().to_string();

    let result = session.today_reorder_before(ghost, anchor).await;
    assert!(
        matches!(result, Err(FfiError::TaskNotFound { .. })),
        "expected TaskNotFound"
    );
}

#[tokio::test]
async fn test_today_reorder_before_nonexistent_anchor() {
    let session = make_session();
    let pos = sequential_positions(1);
    let task = create_today_positioned(&session, "Task", &pos[0]).await;
    let ghost = Uuid::new_v4().to_string();

    let result = session.today_reorder_before(task, ghost).await;
    assert!(
        matches!(result, Err(FfiError::TaskNotFound { .. })),
        "expected TaskNotFound"
    );
}

#[tokio::test]
async fn test_today_reorder_to_beginning_nonexistent() {
    let session = make_session();
    let ghost = Uuid::new_v4().to_string();
    let result = session.today_reorder_to_beginning(ghost).await;
    assert!(
        matches!(result, Err(FfiError::TaskNotFound { .. })),
        "expected TaskNotFound"
    );
}

#[tokio::test]
async fn test_today_reorder_to_end_nonexistent() {
    let session = make_session();
    let ghost = Uuid::new_v4().to_string();
    let result = session.today_reorder_to_end(ghost).await;
    assert!(
        matches!(result, Err(FfiError::TaskNotFound { .. })),
        "expected TaskNotFound"
    );
}

#[tokio::test]
async fn test_today_reorder_independent_of_parent() {
    let session = make_session();
    let pos = sequential_positions(2);

    // Two tasks with different parents can still be today-reordered relative to each other.
    let parent1 = Uuid::new_v4().to_string();
    let parent2 = Uuid::new_v4().to_string();
    session
        .create_task(parent1.clone(), "P1".into())
        .await
        .expect("create p1");
    session
        .create_task(parent2.clone(), "P2".into())
        .await
        .expect("create p2");

    let a = Uuid::new_v4().to_string();
    session
        .create_task(a.clone(), "A".into())
        .await
        .expect("create a");
    session
        .mutate_task(
            a.clone(),
            vec![
                TaskMutation::SetParent {
                    uuid: Some(parent1.clone()),
                },
                TaskMutation::SetTodayPosition {
                    value: Some(pos[0].clone()),
                },
            ],
        )
        .await
        .expect("mutate a");

    let b = Uuid::new_v4().to_string();
    session
        .create_task(b.clone(), "B".into())
        .await
        .expect("create b");
    session
        .mutate_task(
            b.clone(),
            vec![
                TaskMutation::SetParent {
                    uuid: Some(parent2.clone()),
                },
                TaskMutation::SetTodayPosition {
                    value: Some(pos[1].clone()),
                },
            ],
        )
        .await
        .expect("mutate b");

    // A and B have different parents but can be reordered in the today view.
    let moved = session
        .today_reorder_after(a.clone(), b.clone())
        .await
        .expect("today_reorder_after across parents");
    let a_pos = moved.today_position.as_deref().expect("today_position");
    assert!(a_pos > pos[1].as_str(), "A should be after B in today view");
}
