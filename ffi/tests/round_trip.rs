//! End-to-end round-trip test exercising the FFI surface via an in-memory
//! SQLite connection (same tables as `test_from_handle_round_trip`).

use rusqlite::Connection;
use taskchampion_ffi::{
    replica_ops::{all_tasks, create_task, get_task, pending_tasks, undo},
    task_ops::mutate_task,
    types::{FfiStatus, TaskMutation},
};
use uuid::Uuid;

/// Create an in-memory connection and set up all required tables.
fn make_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory connection");
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS tc_tasks (
            id          TEXT PRIMARY KEY,
            user_id     TEXT,
            data        TEXT NOT NULL DEFAULT '{}',
            entry_at    TEXT,
            status      TEXT,
            description TEXT,
            priority    TEXT,
            modified_at TEXT,
            due_at      TEXT,
            scheduled_at TEXT,
            start_at    TEXT,
            end_at      TEXT,
            wait_at     TEXT,
            parent_id   TEXT,
            position    TEXT,
            project_id  TEXT
        );
        CREATE TABLE IF NOT EXISTS tc_operations (
            id         TEXT PRIMARY KEY,
            user_id    TEXT,
            data       TEXT NOT NULL,
            created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
        );
        CREATE TABLE IF NOT EXISTS projects (
            id         TEXT PRIMARY KEY,
            name       TEXT,
            user_id    TEXT,
            created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
        );
        CREATE TABLE IF NOT EXISTS tc_tags (
            id      TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            user_id TEXT,
            name    TEXT NOT NULL,
            UNIQUE (task_id, name)
        );
        CREATE TABLE IF NOT EXISTS tc_annotations (
            id          TEXT PRIMARY KEY,
            task_id     TEXT NOT NULL,
            user_id     TEXT,
            entry_at    TEXT NOT NULL,
            description TEXT NOT NULL
        );
        ",
    )
    .expect("create tables");
    conn
}

/// Cast a `*mut sqlite3` to `i64` for FFI.
fn handle_i64(conn: &Connection) -> i64 {
    (unsafe { conn.handle() }) as i64
}

const USER_ID: &str = "00000000-0000-0000-0000-000000000000";

#[test]
fn test_create_and_read() {
    let conn = make_conn();
    let handle = handle_i64(&conn);
    let uuid = Uuid::new_v4().to_string();

    // Create
    let task =
        create_task(handle, USER_ID.into(), uuid.clone(), "Hello FFI".into()).expect("create_task");
    assert_eq!(task.description, "Hello FFI");
    assert!(matches!(task.status, FfiStatus::Pending));

    // Read back
    let fetched = get_task(handle, USER_ID.into(), uuid.clone())
        .expect("get_task")
        .expect("task should exist");
    assert_eq!(fetched.uuid, uuid);
    assert_eq!(fetched.description, "Hello FFI");
}

#[test]
fn test_mutate_description() {
    let conn = make_conn();
    let handle = handle_i64(&conn);
    let uuid = Uuid::new_v4().to_string();

    create_task(handle, USER_ID.into(), uuid.clone(), "Original".into()).expect("create");

    let updated = mutate_task(
        handle,
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
    let conn = make_conn();
    let handle = handle_i64(&conn);

    let uuid1 = Uuid::new_v4().to_string();
    let uuid2 = Uuid::new_v4().to_string();

    create_task(handle, USER_ID.into(), uuid1.clone(), "Task 1".into()).expect("create 1");
    create_task(handle, USER_ID.into(), uuid2.clone(), "Task 2".into()).expect("create 2");

    let pending = pending_tasks(handle, USER_ID.into()).expect("pending_tasks");
    let descs: Vec<&str> = pending.iter().map(|t| t.description.as_str()).collect();
    assert!(descs.contains(&"Task 1"), "Task 1 should be pending");
    assert!(descs.contains(&"Task 2"), "Task 2 should be pending");
}

#[test]
fn test_all_tasks_includes_completed() {
    let conn = make_conn();
    let handle = handle_i64(&conn);
    let uuid = Uuid::new_v4().to_string();

    create_task(handle, USER_ID.into(), uuid.clone(), "Complete me".into()).expect("create");
    mutate_task(
        handle,
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::Done],
    )
    .expect("done");

    let all = all_tasks(handle, USER_ID.into()).expect("all_tasks");
    let task = all
        .iter()
        .find(|t| t.uuid == uuid)
        .expect("task in all_tasks");
    assert!(matches!(task.status, FfiStatus::Completed));
}

#[test]
fn test_undo_reverses_last_mutation() {
    let conn = make_conn();
    let handle = handle_i64(&conn);
    let uuid = Uuid::new_v4().to_string();

    create_task(handle, USER_ID.into(), uuid.clone(), "Original".into()).expect("create");

    mutate_task(
        handle,
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::SetDescription {
            value: "Changed".into(),
        }],
    )
    .expect("mutate");

    // Verify mutation applied
    let task = get_task(handle, USER_ID.into(), uuid.clone())
        .expect("get_task ok")
        .expect("task exists");
    assert_eq!(task.description, "Changed");

    // Undo should now succeed
    let undone = undo(handle, USER_ID.into()).expect("undo must not error");
    assert!(undone, "undo should return true after mutation");

    // Verify task reverted to original description
    let task = get_task(handle, USER_ID.into(), uuid.clone())
        .expect("get_task ok")
        .expect("task exists after undo");
    assert_eq!(task.description, "Original");
}

#[test]
fn test_add_and_remove_tag() {
    let conn = make_conn();
    let handle = handle_i64(&conn);
    let uuid = Uuid::new_v4().to_string();

    create_task(handle, USER_ID.into(), uuid.clone(), "Tag test".into()).expect("create");

    mutate_task(
        handle,
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::AddTag { tag: "work".into() }],
    )
    .expect("add tag");

    let with_tag = get_task(handle, USER_ID.into(), uuid.clone())
        .expect("get")
        .expect("exists");
    assert!(with_tag.tags.contains(&"work".to_string()));

    mutate_task(
        handle,
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::RemoveTag { tag: "work".into() }],
    )
    .expect("remove tag");

    let without_tag = get_task(handle, USER_ID.into(), uuid.clone())
        .expect("get")
        .expect("exists");
    assert!(!without_tag.tags.contains(&"work".to_string()));
}

#[test]
fn test_handle_not_closed_on_drop() {
    // Verify the FFI storage doesn't close the original connection handle.
    let conn = make_conn();
    let handle = handle_i64(&conn);
    let uuid = Uuid::new_v4().to_string();

    // FFI call creates + drops DirectPowerSyncStorage internally
    create_task(
        handle,
        USER_ID.into(),
        uuid.clone(),
        "Ownership test".into(),
    )
    .expect("create");

    // Original connection still works after FFI storage was dropped
    let still_works: i64 = conn.query_row("SELECT 1", [], |r| r.get(0)).unwrap();
    assert_eq!(still_works, 1, "original connection must survive FFI call");

    // Data written via FFI is visible through the original connection
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM tc_tasks WHERE id = ?", [&uuid], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        count, 1,
        "task written via FFI should be visible to original conn"
    );
}

#[test]
fn test_set_due_round_trip() {
    // Guards the set_value("due", epoch_string) workaround — verifies that a
    // due date written via SetDue is read back as the same epoch value.
    let conn = make_conn();
    let handle = handle_i64(&conn);
    let uuid = Uuid::new_v4().to_string();
    let epoch: i64 = 1_700_000_000; // 2023-11-14 ~ fixed value for determinism

    create_task(handle, USER_ID.into(), uuid.clone(), "Due test".into()).expect("create");

    mutate_task(
        handle,
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::SetDue { epoch: Some(epoch) }],
    )
    .expect("set due");

    let task = get_task(handle, USER_ID.into(), uuid.clone())
        .expect("get")
        .expect("exists");
    assert_eq!(task.due, Some(epoch), "due round-trip via set_value");

    // Clear the due date
    mutate_task(
        handle,
        USER_ID.into(),
        uuid.clone(),
        vec![TaskMutation::SetDue { epoch: None }],
    )
    .expect("clear due");

    let cleared = get_task(handle, USER_ID.into(), uuid)
        .expect("get after clear")
        .expect("exists after clear");
    assert_eq!(cleared.due, None, "due should be None after clearing");
}
