//! Callback-based storage backend.
//!
//! `ExternalStorage` delegates all SQL execution to a host-provided
//! [`SqlExecutor`]. Reads execute immediately; writes buffer in a
//! `Vec<SqlStatement>` and flush atomically on `commit()`.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use uuid::Uuid;

use crate::errors::{Error, Result};
use crate::operation::Operation;
use crate::storage::columns::{raw_to_task, RawTaskRow, TASK_SELECT_COLS};
use crate::storage::sql_ops::{
    add_operation_stmt, create_task_stmt, delete_task_stmts, insert_project_stmt, prepare_task,
    remove_operation_stmt, set_task_stmts, SqlParam, SqlStatement, ALL_OPERATIONS_SQL,
    ALL_OPS_WITH_ID_DESC_SQL, ALL_TASK_UUIDS_SQL, ANNOTATION_QUERY_SQL, PROJECT_LOOKUP_SQL,
    TAG_QUERY_SQL, TASK_EXISTS_SQL,
};
use crate::storage::{Storage, StorageTxn, TaskMap};

// ── SqlExecutor trait ─────────────────────────────────────────────────────

/// Callback interface for host-side SQL execution.
///
/// Implementors run SQL against their own database connection. Methods are
/// async to support non-blocking host-side execution (e.g. Swift async/await).
#[async_trait]
pub trait SqlExecutor: Send + Sync {
    /// Execute a read query returning at most one row as a JSON object string.
    /// Returns `Ok(None)` if no rows match.
    async fn query_one(&self, sql: &str, params: &[SqlParam]) -> Result<Option<String>>;

    /// Execute a read query returning all matching rows as JSON object strings.
    async fn query_all(&self, sql: &str, params: &[SqlParam]) -> Result<Vec<String>>;

    /// Execute a batch of write statements atomically.
    /// The host MUST wrap these in a transaction.
    async fn execute_batch(&self, statements: &[SqlStatement]) -> Result<()>;
}

// ── ExternalStorage ───────────────────────────────────────────────────────

pub struct ExternalStorage {
    executor: Box<dyn SqlExecutor>,
    user_id: Uuid,
}

impl ExternalStorage {
    pub fn new(executor: Box<dyn SqlExecutor>, user_id: Uuid) -> Self {
        Self { executor, user_id }
    }
}

#[async_trait]
impl Storage for ExternalStorage {
    async fn txn<'a>(&'a mut self) -> Result<Box<dyn StorageTxn + Send + 'a>> {
        Ok(Box::new(ExternalStorageTxn {
            executor: &*self.executor,
            user_id: self.user_id,
            write_buffer: Vec::new(),
            project_cache: HashMap::new(),
            pending_creates: HashSet::new(),
            pending_op_deletes: HashSet::new(),
            task_write_cache: HashMap::new(),
        }))
    }
}

// ── ExternalStorageTxn ────────────────────────────────────────────────────

struct ExternalStorageTxn<'a> {
    executor: &'a dyn SqlExecutor,
    user_id: Uuid,
    write_buffer: Vec<SqlStatement>,
    /// Cache project_name → project_id for buffered inserts within this txn.
    project_cache: HashMap<String, String>,
    /// Track UUIDs of tasks created in this uncommitted transaction.
    /// Prevents double-INSERT when `create_task` + `set_task` are called
    /// for the same UUID in one transaction (the shared test suite does this).
    pending_creates: HashSet<Uuid>,
    /// Track IDs of operations deleted in this transaction's write buffer.
    /// Since writes are batched and reads go directly to committed DB state,
    /// we must track buffered deletes locally so that `remove_operation`
    /// sees the correct "effective last" operation across multiple removals
    /// within the same transaction (e.g. during undo).
    pending_op_deletes: HashSet<String>,
    /// Write-ahead cache: stores the latest in-memory state of tasks that
    /// have been written via `set_task` in this transaction.  Since reads go
    /// directly to the committed DB state, consecutive `get_task` / `set_task`
    /// pairs within the same transaction (e.g. inside `apply_op` during undo)
    /// would otherwise lose earlier buffered mutations.  By returning the
    /// cached state here we provide read-your-writes semantics within a
    /// single transaction.
    task_write_cache: HashMap<Uuid, TaskMap>,
}

impl ExternalStorageTxn<'_> {
    /// Resolve a project name to its ID. Checks local cache first (for
    /// projects created in this uncommitted transaction), then queries the host.
    async fn resolve_project_id(&mut self, name: &str) -> Result<String> {
        // Check local cache first.
        if let Some(id) = self.project_cache.get(name) {
            return Ok(id.clone());
        }

        // Query host.
        let row = self
            .executor
            .query_one(PROJECT_LOOKUP_SQL, &[SqlParam::Text(name.to_string())])
            .await?;

        if let Some(json) = row {
            let id = parse_json_string_field(&json, "id")?;
            self.project_cache.insert(name.to_string(), id.clone());
            return Ok(id);
        }

        // Not found — generate ID, buffer INSERT, cache it.
        let new_id = Uuid::new_v4();
        self.write_buffer
            .push(insert_project_stmt(&new_id, name, &self.user_id));
        let new_id_str = new_id.to_string();
        self.project_cache
            .insert(name.to_string(), new_id_str.clone());
        Ok(new_id_str)
    }

    /// Parse a JSON row into a `RawTaskRow`.
    fn parse_task_row(json: &str) -> Result<RawTaskRow> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| Error::Database(format!("Failed to parse task row JSON: {e}")))?;
        let obj = v
            .as_object()
            .ok_or_else(|| Error::Database("Expected JSON object for task row".into()))?;

        // Required fields — missing or null means the host returned a broken row.
        let id = get_opt_str(obj, "id")
            .ok_or_else(|| Error::Database("Missing 'id' in task row".into()))?;
        let data = get_opt_str(obj, "data")
            .ok_or_else(|| Error::Database("Missing 'data' in task row".into()))?;

        Ok(RawTaskRow {
            id,
            data,
            status: get_opt_str(obj, "status"),
            description: get_opt_str(obj, "description"),
            priority: get_opt_str(obj, "priority"),
            entry_at: get_opt_str(obj, "entry_at"),
            modified_at: get_opt_str(obj, "modified_at"),
            due_at: get_opt_str(obj, "due_at"),
            scheduled_at: get_opt_str(obj, "scheduled_at"),
            start_at: get_opt_str(obj, "start_at"),
            end_at: get_opt_str(obj, "end_at"),
            wait_at: get_opt_str(obj, "wait_at"),
            parent_id: get_opt_str(obj, "parent_id"),
            position: get_opt_str(obj, "position"),
            project_name: get_opt_str(obj, "project_name"),
        })
    }

    /// Merge tags and annotations from the host DB into a TaskMap.
    async fn merge_tags_annotations(&self, task_id: &str, task_map: &mut TaskMap) -> Result<()> {
        // Tags.
        let tag_rows = self
            .executor
            .query_all(TAG_QUERY_SQL, &[SqlParam::Text(task_id.to_string())])
            .await?;
        for json in &tag_rows {
            let name = parse_json_string_field(json, "name")?;
            task_map.insert(format!("tag_{name}"), String::new());
        }

        // Annotations.
        let ann_rows = self
            .executor
            .query_all(ANNOTATION_QUERY_SQL, &[SqlParam::Text(task_id.to_string())])
            .await?;
        for json in &ann_rows {
            let entry_at_iso = parse_json_string_field(json, "entry_at")?;
            let description = parse_json_string_field(json, "description")?;
            let dt = chrono::DateTime::parse_from_rfc3339(&entry_at_iso).map_err(|e| {
                Error::Database(format!(
                    "Invalid annotation timestamp for task {task_id}: {entry_at_iso:?}: {e}"
                ))
            })?;
            task_map.insert(format!("annotation_{}", dt.timestamp()), description);
        }

        Ok(())
    }
}

// ── StorageTxn impl ───────────────────────────────────────────────────────

#[async_trait]
impl StorageTxn for ExternalStorageTxn<'_> {
    async fn get_task(&mut self, uuid: Uuid) -> Result<Option<TaskMap>> {
        // Read-your-writes: return the most recent in-transaction state.
        if let Some(task) = self.task_write_cache.get(&uuid) {
            return Ok(Some(task.clone()));
        }
        // A task created in this transaction is in the write buffer but not yet
        // visible to the host DB. Return an empty TaskMap so that subsequent
        // Update operations within the same transaction can build on it.
        if self.pending_creates.contains(&uuid) {
            return Ok(Some(TaskMap::new()));
        }
        let sql = format!(
            "SELECT {TASK_SELECT_COLS} FROM tc_tasks t \
             LEFT JOIN projects p ON t.project_id = p.id \
             WHERE t.id = ? LIMIT 1"
        );
        let row = self
            .executor
            .query_one(&sql, &[SqlParam::Text(uuid.to_string())])
            .await?;
        match row {
            None => Ok(None),
            Some(json) => {
                let raw = Self::parse_task_row(&json)?;
                let (_, mut task_map) = raw_to_task(raw)?;
                self.merge_tags_annotations(&uuid.to_string(), &mut task_map)
                    .await?;
                Ok(Some(task_map))
            }
        }
    }

    async fn get_pending_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let sql = format!(
            "SELECT {TASK_SELECT_COLS} FROM tc_tasks t \
             LEFT JOIN projects p ON t.project_id = p.id \
             WHERE t.status = 'pending'"
        );
        let rows = self.executor.query_all(&sql, &[]).await?;
        let mut tasks = Vec::new();
        for json in &rows {
            let raw = Self::parse_task_row(json)?;
            let (uuid, mut task_map) = raw_to_task(raw)?;
            self.merge_tags_annotations(&uuid.to_string(), &mut task_map)
                .await?;
            tasks.push((uuid, task_map));
        }
        Ok(tasks)
    }

    async fn create_task(&mut self, uuid: Uuid) -> Result<bool> {
        // Check pending_creates first (uncommitted creates in this txn).
        if self.pending_creates.contains(&uuid) {
            return Ok(false);
        }
        let exists_json = self
            .executor
            .query_one(TASK_EXISTS_SQL, &[SqlParam::Text(uuid.to_string())])
            .await?;
        if parse_json_bool(&exists_json, "exists_flag")? {
            return Ok(false);
        }
        self.write_buffer
            .push(create_task_stmt(&uuid, &self.user_id));
        self.pending_creates.insert(uuid);
        Ok(true)
    }

    async fn set_task(&mut self, uuid: Uuid, task: TaskMap) -> Result<()> {
        // Cache the task map so subsequent get_task calls within this
        // transaction see the buffered state (read-your-writes).
        self.task_write_cache.insert(uuid, task.clone());
        let prepared = prepare_task(task)?;

        // Resolve project (uses local cache for buffered projects).
        let project_id: Option<String> = match &prepared.project_name {
            Some(name) => Some(self.resolve_project_id(name).await?),
            None => None,
        };

        // Check existence: pending_creates first, then host DB.
        let exists = if self.pending_creates.contains(&uuid) {
            true
        } else {
            let exists_json = self
                .executor
                .query_one(TASK_EXISTS_SQL, &[SqlParam::Text(uuid.to_string())])
                .await?;
            parse_json_bool(&exists_json, "exists_flag")?
        };

        // Buffer write statements.
        self.write_buffer.extend(set_task_stmts(
            &uuid,
            &prepared,
            &self.user_id,
            exists,
            project_id.as_deref(),
        )?);
        Ok(())
    }

    async fn delete_task(&mut self, uuid: Uuid) -> Result<bool> {
        // Evict from write cache — task no longer exists in this transaction.
        self.task_write_cache.remove(&uuid);
        // Check pending_creates first.
        let exists = if self.pending_creates.contains(&uuid) {
            true
        } else {
            let exists_json = self
                .executor
                .query_one(TASK_EXISTS_SQL, &[SqlParam::Text(uuid.to_string())])
                .await?;
            parse_json_bool(&exists_json, "exists_flag")?
        };
        if exists {
            self.write_buffer.extend(delete_task_stmts(&uuid));
            self.pending_creates.remove(&uuid);
        }
        Ok(exists)
    }

    async fn all_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let sql = format!(
            "SELECT {TASK_SELECT_COLS} FROM tc_tasks t \
             LEFT JOIN projects p ON t.project_id = p.id"
        );
        let rows = self.executor.query_all(&sql, &[]).await?;
        let mut tasks = Vec::new();
        for json in &rows {
            let raw = Self::parse_task_row(json)?;
            let (uuid, mut task_map) = raw_to_task(raw)?;
            self.merge_tags_annotations(&uuid.to_string(), &mut task_map)
                .await?;
            tasks.push((uuid, task_map));
        }
        Ok(tasks)
    }

    async fn all_task_uuids(&mut self) -> Result<Vec<Uuid>> {
        let rows = self.executor.query_all(ALL_TASK_UUIDS_SQL, &[]).await?;
        rows.iter()
            .map(|json| {
                let id = parse_json_string_field(json, "id")?;
                Uuid::parse_str(&id).map_err(|e| Error::Database(format!("Invalid UUID: {e}")))
            })
            .collect()
    }

    async fn get_task_operations(&mut self, uuid: Uuid) -> Result<Vec<Operation>> {
        let rows = self.executor.query_all(ALL_OPERATIONS_SQL, &[]).await?;
        rows.iter()
            .map(|json| {
                let data_str = parse_json_string_field(json, "data")?;
                serde_json::from_str::<Operation>(&data_str)
                    .map_err(|e| Error::Database(format!("Failed to parse operation: {e}")))
            })
            .filter_map(|res| match res {
                Ok(op) if op.get_uuid() == Some(uuid) => Some(Ok(op)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    async fn all_operations(&mut self) -> Result<Vec<Operation>> {
        let rows = self.executor.query_all(ALL_OPERATIONS_SQL, &[]).await?;
        rows.iter()
            .map(|json| {
                let data_str = parse_json_string_field(json, "data")?;
                serde_json::from_str::<Operation>(&data_str)
                    .map_err(|e| Error::Database(format!("Failed to parse operation: {e}")))
            })
            .collect()
    }

    async fn add_operation(&mut self, op: Operation) -> Result<()> {
        self.write_buffer
            .push(add_operation_stmt(&op, &self.user_id)?);
        Ok(())
    }

    async fn remove_operation(&mut self, op: Operation) -> Result<()> {
        // Read all operations ordered newest-first and find the effective last,
        // skipping any that have already been buffered for deletion in this
        // transaction (reads see committed state; buffered deletes are not yet
        // visible to the host DB).
        let rows = self
            .executor
            .query_all(ALL_OPS_WITH_ID_DESC_SQL, &[])
            .await?;
        let effective_last = rows.iter().find(|json| {
            parse_json_string_field(json, "id")
                .map(|id| !self.pending_op_deletes.contains(&id))
                .unwrap_or(false)
        });
        let Some(json) = effective_last else {
            return Err(Error::Database("No operations to remove".into()));
        };
        let last_id = parse_json_string_field(json, "id")?;
        let last_data = parse_json_string_field(json, "data")?;
        let last_op: Operation = serde_json::from_str(&last_data)
            .map_err(|e| Error::Database(format!("Failed to parse operation: {e}")))?;
        if last_op != op {
            return Err(Error::Database(format!(
                "Last operation does not match -- cannot remove \
                 (expected {op:?}, got {last_op:?})"
            )));
        }
        self.write_buffer.push(remove_operation_stmt(&last_id));
        self.pending_op_deletes.insert(last_id);
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        if !self.write_buffer.is_empty() {
            self.executor
                .execute_batch(&std::mem::take(&mut self.write_buffer))
                .await?;
        }
        Ok(())
    }
}

// ── JSON parsing helpers ──────────────────────────────────────────────────

/// Extract an optional string field from a JSON object. Returns `None` for
/// missing or null fields; non-string types also return `None`.
fn get_opt_str(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| match v {
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    })
}

/// Extract a required string field from a JSON object string.
/// Returns `Err` if the field is missing, null, or not a string type.
fn parse_json_string_field(json: &str, field: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| Error::Database(format!("Failed to parse JSON: {e}")))?;
    match v.get(field) {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(serde_json::Value::Null) | None => Err(Error::Database(format!(
            "Missing field {field:?} in JSON row"
        ))),
        Some(other) => Err(Error::Database(format!(
            "Field {field:?} must be a string, got: {other}"
        ))),
    }
}

/// Parse the named field from a JSON row as a boolean/integer flag.
/// Returns `Err` if the row is missing, if the field is absent, or if
/// the field value is neither a bool nor an integer.
fn parse_json_bool(json: &Option<String>, field: &str) -> Result<bool> {
    let Some(json) = json else {
        return Ok(false);
    };
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| Error::Database(format!("Failed to parse JSON: {e}")))?;
    match v.get(field) {
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(serde_json::Value::Number(n)) => Ok(n.as_i64().unwrap_or(0) != 0),
        Some(other) => Err(Error::Database(format!(
            "Field {field:?} must be a number, got: {other}"
        ))),
        None => Err(Error::Database(format!(
            "Missing field {field:?} in result"
        ))),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "storage-powersync"))]
mod test {
    use super::*;
    use crate::storage::test::storage_tests_no_sync;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Mock executor backed by an in-memory SQLite connection.
    /// Proves ExternalStorage works correctly by running the same test
    /// suite as PowerSyncStorage.
    struct MockSqlExecutor {
        conn: Mutex<rusqlite::Connection>,
    }

    impl MockSqlExecutor {
        fn new() -> Self {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
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
            .unwrap();
            Self {
                conn: Mutex::new(conn),
            }
        }

        /// Convert a rusqlite Row to a JSON object string.
        /// Uses `value_ref()` to handle all SQLite native types correctly
        /// (Integer, Real, Text, Null) — avoids silent coercion failures
        /// where `row.get::<_, String>` would fail for integer columns.
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
    }

    #[async_trait]
    impl SqlExecutor for MockSqlExecutor {
        async fn query_one(&self, sql: &str, params: &[SqlParam]) -> Result<Option<String>> {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| Error::Database(format!("Prepare failed: {e}")))?;
            let col_count = stmt.column_count();
            let result = stmt.query_row(rusqlite::params_from_iter(params.iter()), |row| {
                Self::row_to_json(row, col_count)
            });
            match result {
                Ok(json) => Ok(Some(json)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(Error::Database(format!("Query failed: {e}"))),
            }
        }

        async fn query_all(&self, sql: &str, params: &[SqlParam]) -> Result<Vec<String>> {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn
                .prepare(sql)
                .map_err(|e| Error::Database(format!("Prepare failed: {e}")))?;
            let col_count = stmt.column_count();
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                    Self::row_to_json(row, col_count)
                })
                .map_err(|e| Error::Database(format!("Query failed: {e}")))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| Error::Database(format!("Row read failed: {e}")))
        }

        async fn execute_batch(&self, statements: &[SqlStatement]) -> Result<()> {
            let mut conn = self.conn.lock().unwrap();
            let txn = conn
                .transaction()
                .map_err(|e| Error::Database(format!("Begin txn failed: {e}")))?;
            for stmt in statements {
                txn.execute(&stmt.sql, rusqlite::params_from_iter(stmt.params.iter()))
                    .map_err(|e| {
                        Error::Database(format!("Execute failed: {e} (sql: {})", stmt.sql))
                    })?;
            }
            txn.commit()
                .map_err(|e| Error::Database(format!("Commit failed: {e}")))?;
            Ok(())
        }
    }

    async fn storage() -> ExternalStorage {
        ExternalStorage::new(Box::new(MockSqlExecutor::new()), Uuid::nil())
    }

    // Run the shared test suite.
    storage_tests_no_sync!(storage().await);

    // ── ExternalStorage-specific tests ────────────────────────────────────

    /// Verify tags round-trip through ExternalStorage.
    #[tokio::test]
    async fn test_external_tags_round_trip() {
        let mut storage = storage().await;
        let uuid = Uuid::new_v4();

        let mut txn = storage.txn().await.unwrap();
        txn.create_task(uuid).await.unwrap();
        let mut task = TaskMap::new();
        task.insert("status".into(), "pending".into());
        task.insert("tag_work".into(), String::new());
        task.insert("tag_urgent".into(), String::new());
        txn.set_task(uuid, task).await.unwrap();
        txn.commit().await.unwrap();
        drop(txn);

        let mut txn = storage.txn().await.unwrap();
        let got = txn.get_task(uuid).await.unwrap().unwrap();
        assert_eq!(got.get("tag_work").map(String::as_str), Some(""));
        assert_eq!(got.get("tag_urgent").map(String::as_str), Some(""));
    }

    /// Verify annotations round-trip through ExternalStorage.
    #[tokio::test]
    async fn test_external_annotations_round_trip() {
        let mut storage = storage().await;
        let uuid = Uuid::new_v4();

        let mut txn = storage.txn().await.unwrap();
        txn.create_task(uuid).await.unwrap();
        let mut task = TaskMap::new();
        task.insert("status".into(), "pending".into());
        task.insert("annotation_1635301873".into(), "pick up groceries".into());
        txn.set_task(uuid, task).await.unwrap();
        txn.commit().await.unwrap();
        drop(txn);

        let mut txn = storage.txn().await.unwrap();
        let got = txn.get_task(uuid).await.unwrap().unwrap();
        assert_eq!(
            got.get("annotation_1635301873").map(String::as_str),
            Some("pick up groceries")
        );
    }

    /// Verify project name round-trips and the local cache prevents duplicates.
    #[tokio::test]
    async fn test_external_project_round_trip() {
        let mut storage = storage().await;

        let uuid1 = Uuid::new_v4();
        let uuid2 = Uuid::new_v4();

        let mut txn = storage.txn().await.unwrap();
        let mut task1 = TaskMap::new();
        task1.insert("project".into(), "home".into());
        txn.set_task(uuid1, task1).await.unwrap();
        let mut task2 = TaskMap::new();
        task2.insert("project".into(), "home".into());
        txn.set_task(uuid2, task2).await.unwrap();
        txn.commit().await.unwrap();
        drop(txn);

        let mut txn = storage.txn().await.unwrap();
        let got1 = txn.get_task(uuid1).await.unwrap().unwrap();
        let got2 = txn.get_task(uuid2).await.unwrap().unwrap();
        assert_eq!(got1.get("project").map(String::as_str), Some("home"));
        assert_eq!(got2.get("project").map(String::as_str), Some("home"));
    }

    /// Verify write buffer is not flushed if commit is not called (drop = abort).
    #[tokio::test]
    async fn test_external_drop_aborts_transaction() {
        let mut storage = storage().await;
        let uuid = Uuid::new_v4();

        {
            let mut txn = storage.txn().await.unwrap();
            txn.create_task(uuid).await.unwrap();
            // drop without commit
        }

        let mut txn = storage.txn().await.unwrap();
        assert!(txn.get_task(uuid).await.unwrap().is_none());
    }

    /// Verify timestamp columns round-trip through ExternalStorage.
    #[tokio::test]
    async fn test_external_timestamps_round_trip() {
        let mut storage = storage().await;
        let uuid = Uuid::new_v4();
        let epoch = "1724612771";

        let mut txn = storage.txn().await.unwrap();
        let mut task = TaskMap::new();
        for key in [
            "entry",
            "modified",
            "due",
            "scheduled",
            "start",
            "end",
            "wait",
        ] {
            task.insert(key.into(), epoch.into());
        }
        txn.set_task(uuid, task).await.unwrap();
        txn.commit().await.unwrap();
        drop(txn);

        let mut txn = storage.txn().await.unwrap();
        let got = txn.get_task(uuid).await.unwrap().unwrap();
        for key in [
            "entry",
            "modified",
            "due",
            "scheduled",
            "start",
            "end",
            "wait",
        ] {
            assert_eq!(got.get(key).map(String::as_str), Some(epoch), "field {key}");
        }
    }

    /// Verify get_pending_tasks returns only status='pending' tasks with full data.
    #[tokio::test]
    async fn test_external_get_pending_tasks() {
        let mut storage = storage().await;
        let uuid_pending = Uuid::new_v4();
        let uuid_completed = Uuid::new_v4();

        let mut txn = storage.txn().await.unwrap();
        txn.create_task(uuid_pending).await.unwrap();
        let mut t1 = TaskMap::new();
        t1.insert("status".into(), "pending".into());
        t1.insert("description".into(), "pending task".into());
        t1.insert("tag_work".into(), String::new());
        txn.set_task(uuid_pending, t1).await.unwrap();

        txn.create_task(uuid_completed).await.unwrap();
        let mut t2 = TaskMap::new();
        t2.insert("status".into(), "completed".into());
        txn.set_task(uuid_completed, t2).await.unwrap();
        txn.commit().await.unwrap();
        drop(txn);

        let mut txn = storage.txn().await.unwrap();
        let pending = txn.get_pending_tasks().await.unwrap();
        assert_eq!(pending.len(), 1, "should return exactly one pending task");
        let (got_uuid, got_task) = &pending[0];
        assert_eq!(*got_uuid, uuid_pending);
        assert_eq!(got_task.get("status").map(String::as_str), Some("pending"));
        assert_eq!(
            got_task.get("description").map(String::as_str),
            Some("pending task")
        );
        // Tags must be merged in for pending tasks too.
        assert_eq!(got_task.get("tag_work").map(String::as_str), Some(""));
    }

    /// Verify remove_operation errors when the operation log is empty.
    #[tokio::test]
    async fn test_external_remove_operation_empty() {
        let mut storage = storage().await;
        let mut txn = storage.txn().await.unwrap();
        let result = txn.remove_operation(Operation::UndoPoint).await;
        assert!(result.is_err(), "should error on empty op log");
        assert!(
            result.unwrap_err().to_string().contains("No operations"),
            "error message should mention empty log"
        );
    }

    /// Verify remove_operation errors when the last op doesn't match.
    #[tokio::test]
    async fn test_external_remove_operation_mismatch() {
        let mut storage = storage().await;
        let uuid = Uuid::new_v4();

        let mut txn = storage.txn().await.unwrap();
        txn.add_operation(Operation::Create { uuid }).await.unwrap();
        txn.commit().await.unwrap();
        drop(txn);

        // Try to remove UndoPoint, but the last op is Create.
        let mut txn = storage.txn().await.unwrap();
        let result = txn.remove_operation(Operation::UndoPoint).await;
        assert!(result.is_err(), "should error on mismatch");
        assert!(
            result.unwrap_err().to_string().contains("does not match"),
            "error message should mention mismatch"
        );
    }
}
