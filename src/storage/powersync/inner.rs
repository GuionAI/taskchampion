use crate::errors::{Error, Result};
use crate::operation::Operation;
use crate::storage::send_wrapper::{WrappedStorage, WrappedStorageTxn};
use crate::storage::TaskMap;
use anyhow::Context;
use async_trait::async_trait;
use chrono::DateTime;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;
use uuid::Uuid;

use super::extension::init_powersync_extension;
use super::row_reader::{query_task_rows, read_raw_task_row};
use crate::storage::columns::{raw_to_task, TASK_SELECT_COLS};
use crate::storage::sql_ops::{
    add_operation_stmt, create_task_stmt, delete_task_stmts, prepare_task, remove_operation_stmt,
    set_tag_color_stmt, set_task_stmts, SqlStatement, ALL_OPERATIONS_SQL, ALL_TASK_UUIDS_SQL,
    LAST_OPERATION_SQL, TAG_COLOR_READ_SQL, TASK_EXISTS_SQL,
};

/// Query tc_tags and tc_annotations for the given task UUID and inject them
/// into the TaskMap as `tag_<name>` and `annotation_<epoch>` keys.
fn merge_tags_annotations(
    t: &rusqlite::Transaction<'_>,
    task_id: &str,
    task_map: &mut TaskMap,
) -> Result<()> {
    let mut tag_stmt = t
        .prepare("SELECT name FROM tc_tags WHERE task_id = ?")
        .context("Prepare tag query")?;
    let tag_rows = tag_stmt
        .query_map([task_id], |row| row.get::<_, String>(0))
        .context("Query tags")?;
    for name in tag_rows {
        let name = name?;
        task_map.insert(format!("tag_{name}"), String::new());
    }

    let mut ann_stmt = t
        .prepare("SELECT entry_at, description FROM tc_annotations WHERE task_id = ?")
        .context("Prepare annotation query")?;
    let ann_rows = ann_stmt
        .query_map([task_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("Query annotations")?;
    for row in ann_rows {
        let (entry_at_iso, description) = row?;
        let dt = DateTime::parse_from_rfc3339(&entry_at_iso).map_err(|e| {
            Error::Database(format!(
                "Invalid annotation timestamp for task {task_id}: {entry_at_iso:?}: {e}"
            ))
        })?;
        task_map.insert(format!("annotation_{}", dt.timestamp()), description);
    }

    Ok(())
}

/// Execute a SqlStatement against a rusqlite Transaction.
fn execute_sql_stmt(t: &rusqlite::Transaction, stmt: &SqlStatement) -> Result<()> {
    t.execute(&stmt.sql, rusqlite::params_from_iter(stmt.params.iter()))
        .context("Executing SQL statement")?;
    Ok(())
}

pub(super) struct PowerSyncStorageInner {
    pub(super) conn: Connection,
    user_id: Uuid,
}

impl PowerSyncStorageInner {
    /// Open an existing PowerSync-managed database file and create local-only tables.
    pub(super) fn new(db_path: &Path, user_id: Uuid) -> Result<Self> {
        // Register the PowerSync extension as a SQLite auto-extension (once per process).
        init_powersync_extension()?;

        // Open the connection. The auto-extension fires on open, registering all
        // PowerSync functions (powersync_strip_subtype, etc.).
        let conn = Connection::open(db_path)
            .context("Opening PowerSync database (auto-extension init fires here)")?;

        // Verify the DB has been initialized by flicknote-sync (tc_tasks view must exist).
        let has_tc_tasks: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='view' AND name='tc_tasks'",
                [],
                |r| r.get(0),
            )
            .context("Checking for tc_tasks view")?;
        if !has_tc_tasks {
            return Err(Error::Database(
                "tc_tasks view not found — the database must be initialized by flicknote-sync \
                 before flicktask can use it. Run flicknote-sync first to set up PowerSync views."
                    .into(),
            ));
        }

        // Belt-and-suspenders: ensure WAL mode and busy_timeout for multi-process safety.
        // flicknote-sync already sets WAL (persists in DB header), but set it explicitly
        // in case it was somehow reset. busy_timeout is per-connection — must always be set.
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("Setting WAL mode")?;
        conn.pragma_update(None, "busy_timeout", 30_000)
            .context("Setting busy timeout")?;

        // Initialize PowerSync internal tables (ps_migration, ps_oplog, etc.).
        // This does NOT create user-facing views — those already exist from flicknote-sync.
        // We intentionally do NOT call powersync_replace_schema here because it performs
        // a FULL REPLACE — it would drop views for notes, projects, note_extractions
        // that flicknote-sync registered. We only need the extension functions loaded
        // (which happened at Connection::open via auto-extension).
        conn.prepare("SELECT powersync_init()")?
            .query_row([], |_| Ok(()))
            .context("PowerSync init")?;

        // No local-only tables needed: tc_tasks and tc_operations are PowerSync-managed
        // views; sync state (working-set, base_version, operations_sync) is unused since
        // PowerSync handles replication externally via flicknote-sync.

        Ok(Self { conn, user_id })
    }

    /// Create an in-memory database with all required tables for testing.
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn new_for_test() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS tc_tasks (
                id TEXT PRIMARY KEY,
                user_id TEXT,
                data TEXT NOT NULL DEFAULT '{}',
                entry_at TEXT,
                status TEXT,
                description TEXT,
                priority TEXT,
                modified_at TEXT,
                due_at TEXT,
                scheduled_at TEXT,
                start_at TEXT,
                end_at TEXT,
                wait_at TEXT,
                parent_id TEXT,
                position TEXT,
                project_id TEXT
            );
            CREATE TABLE IF NOT EXISTS tc_operations (
                id TEXT PRIMARY KEY,
                user_id TEXT,
                data TEXT NOT NULL,
                created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
            );
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                name TEXT,
                user_id TEXT,
                created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
            );
            CREATE TABLE IF NOT EXISTS tc_tags (
                id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                user_id TEXT,
                name TEXT NOT NULL,
                UNIQUE (task_id, name)
            );
            CREATE TABLE IF NOT EXISTS tc_annotations (
                id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                user_id TEXT,
                entry_at TEXT NOT NULL,
                description TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tc_tag_colors (
                id TEXT PRIMARY KEY,
                user_id TEXT,
                name TEXT NOT NULL,
                color TEXT NOT NULL,
                created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
            );
        ",
        )
        .context("Creating PowerSync test tables")?;
        Ok(Self {
            conn,
            user_id: Uuid::nil(),
        })
    }
}

#[async_trait(?Send)]
impl WrappedStorage for PowerSyncStorageInner {
    async fn txn<'a>(&'a mut self) -> Result<Box<dyn WrappedStorageTxn + 'a>> {
        let txn = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Ok(Box::new(PowerSyncTxn {
            txn: Some(txn),
            user_id: self.user_id,
        }))
    }
}

pub(super) struct PowerSyncTxn<'t> {
    txn: Option<rusqlite::Transaction<'t>>,
    user_id: Uuid,
}

impl<'t> PowerSyncTxn<'t> {
    fn get_txn(&self) -> Result<&rusqlite::Transaction<'t>> {
        self.txn
            .as_ref()
            .ok_or_else(|| Error::Database("Transaction already committed".into()))
    }

    /// Look up an existing project by name, or insert a new one and return its ID.
    fn resolve_project_id(&self, name: &str) -> Result<String> {
        let t = self.get_txn()?;
        if let Some(id) = t
            .query_row(
                "SELECT id FROM projects WHERE name = ? ORDER BY created_at LIMIT 1",
                [name],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(id);
        }

        // INSTEAD OF triggers on PowerSync views report 0 rows changed,
        // so we can't rely on t.changes() to detect INSERT OR IGNORE behavior.
        let new_id = Uuid::new_v4().to_string();
        t.execute(
            "INSERT OR IGNORE INTO projects (id, name, user_id) VALUES (?, ?, ?)",
            params![&new_id, name, &self.user_id.to_string()],
        )?;

        // Re-query to get the authoritative ID — either the one we just inserted
        // or the existing one if INSERT was ignored.
        t.query_row(
            "SELECT id FROM projects WHERE name = ? ORDER BY created_at LIMIT 1",
            [name],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| Error::Database(format!("Failed to resolve project id for {name:?}")))
    }
}

/// Parse an operation from a JSON string, handling double-encoded JSONB values.
///
/// PowerSync sync from Supabase can double-encode bare JSON string values:
/// `Operation::UndoPoint` serializes as `"UndoPoint"` (a JSON string), which
/// Supabase JSONB stores as a string value. When PowerSync syncs back to SQLite
/// TEXT, it re-serializes the JSONB, producing `"\"UndoPoint\""`. Object variants
/// like `{"Create":{...}}` are unaffected because JSON objects don't get re-wrapped.
fn parse_operation(data_str: &str) -> Result<Operation> {
    match serde_json::from_str::<Operation>(data_str) {
        Ok(op) => Ok(op),
        Err(original_err) => {
            // If the string is a double-encoded JSON value (starts and ends with `"`),
            // unwrap one layer of JSON string encoding and retry.
            if data_str.starts_with('"') && data_str.ends_with('"') {
                if let Ok(inner) = serde_json::from_str::<String>(data_str) {
                    return serde_json::from_str::<Operation>(&inner).map_err(|e| {
                        Error::Database(format!("Failed to parse operation (unwrapped): {e}"))
                    });
                }
            }
            Err(Error::Database(format!(
                "Failed to parse operation: {original_err}"
            )))
        }
    }
}

#[async_trait(?Send)]
impl WrappedStorageTxn for PowerSyncTxn<'_> {
    async fn get_task(&mut self, uuid: Uuid) -> Result<Option<TaskMap>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {TASK_SELECT_COLS}
             FROM tc_tasks t
             LEFT JOIN projects p ON t.project_id = p.id
             WHERE t.id = ? LIMIT 1"
        );
        let raw_opt = t
            .query_row(&sql, [&uuid.to_string()], read_raw_task_row)
            .optional()?;
        match raw_opt {
            None => Ok(None),
            Some(raw) => {
                let (_, mut task_map) = raw_to_task(raw)?;
                merge_tags_annotations(t, &uuid.to_string(), &mut task_map)?;
                Ok(Some(task_map))
            }
        }
    }

    async fn get_pending_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {TASK_SELECT_COLS}
             FROM tc_tasks t
             LEFT JOIN projects p ON t.project_id = p.id
             WHERE t.status = 'pending'"
        );
        let mut tasks = query_task_rows(t, &sql, [])?;
        for (uuid, task_map) in &mut tasks {
            let uuid_str = uuid.to_string();
            merge_tags_annotations(t, &uuid_str, task_map)?;
        }
        Ok(tasks)
    }

    async fn create_task(&mut self, uuid: Uuid) -> Result<bool> {
        let t = self.get_txn()?;
        let count: usize = t.query_row(
            "SELECT count(id) FROM tc_tasks WHERE id = ?",
            [&uuid.to_string()],
            |x| x.get(0),
        )?;
        if count > 0 {
            return Ok(false);
        }
        execute_sql_stmt(t, &create_task_stmt(&uuid, &self.user_id))?;
        Ok(true)
    }

    async fn set_task(&mut self, uuid: Uuid, task: TaskMap) -> Result<()> {
        let prepared = prepare_task(task)?;

        // Resolve project name → project_id (look up or create in projects table).
        let project_id: Option<String> = prepared
            .project_name
            .as_ref()
            .map(|name| self.resolve_project_id(name))
            .transpose()?;

        // PowerSync views don't support UPSERT (INSERT ... ON CONFLICT DO UPDATE).
        // INSTEAD OF triggers also report 0 rows changed regardless of success,
        // so we check existence with SELECT, then INSERT or UPDATE accordingly.
        let t = self.get_txn()?;
        let exists: bool = t
            .query_row(TASK_EXISTS_SQL, [&uuid.to_string()], |row| row.get(0))
            .context("Set task existence check")?;

        // Generate and execute statements.
        let stmts = set_task_stmts(
            &uuid,
            &prepared,
            &self.user_id,
            exists,
            project_id.as_deref(),
        )?;
        for stmt in &stmts {
            execute_sql_stmt(t, stmt)?;
        }
        Ok(())
    }

    async fn delete_task(&mut self, uuid: Uuid) -> Result<bool> {
        let t = self.get_txn()?;
        let uuid_str = uuid.to_string();
        // INSTEAD OF triggers on PowerSync views report 0 rows changed,
        // so check existence before DELETE to return the correct boolean.
        let exists: bool = t
            .query_row(TASK_EXISTS_SQL, [&uuid_str], |row| row.get(0))
            .context("Delete task existence check")?;
        if exists {
            for stmt in &delete_task_stmts(&uuid) {
                execute_sql_stmt(t, stmt)?;
            }
        }
        Ok(exists)
    }

    async fn all_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {TASK_SELECT_COLS}
             FROM tc_tasks t
             LEFT JOIN projects p ON t.project_id = p.id"
        );
        let mut tasks = query_task_rows(t, &sql, [])?;
        for (uuid, task_map) in &mut tasks {
            let uuid_str = uuid.to_string();
            merge_tags_annotations(t, &uuid_str, task_map)?;
        }
        Ok(tasks)
    }

    async fn all_task_uuids(&mut self) -> Result<Vec<Uuid>> {
        let t = self.get_txn()?;
        let mut q = t.prepare(ALL_TASK_UUIDS_SQL)?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|s| Uuid::parse_str(&s).map_err(|e| Error::Database(format!("Invalid UUID: {e}"))))
            .collect()
    }

    async fn get_task_operations(&mut self, uuid: Uuid) -> Result<Vec<Operation>> {
        // tc_operations has no UUID column (schema is PowerSync-managed).
        // Filter in memory after deserializing; acceptable for the expected operation count.
        let t = self.get_txn()?;
        let mut q = t.prepare(ALL_OPERATIONS_SQL)?;
        let rows = q.query_map([], |r| r.get::<_, String>("data"))?;
        let raw: Vec<String> = rows.collect::<std::result::Result<_, _>>()?;
        raw.into_iter()
            .map(|data_str| parse_operation(&data_str))
            .filter_map(|res| match res {
                Ok(op) if op.get_uuid() == Some(uuid) => Some(Ok(op)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    async fn all_operations(&mut self) -> Result<Vec<Operation>> {
        let t = self.get_txn()?;
        let mut q = t.prepare(ALL_OPERATIONS_SQL)?;
        let rows = q.query_map([], |r| r.get::<_, String>("data"))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|data_str| parse_operation(&data_str))
            .collect()
    }

    async fn add_operation(&mut self, op: Operation) -> Result<()> {
        let t = self.get_txn()?;
        execute_sql_stmt(t, &add_operation_stmt(&op, &self.user_id)?)?;
        Ok(())
    }

    async fn remove_operation(&mut self, op: Operation) -> Result<()> {
        let t = self.get_txn()?;
        let last: Option<(String, String)> = t
            .query_row(LAST_OPERATION_SQL, [], |x| Ok((x.get(0)?, x.get(1)?)))
            .optional()?;

        let Some((last_id, last_data)) = last else {
            return Err(Error::Database("No operations to remove".into()));
        };

        let last_op: Operation = parse_operation(&last_data)?;

        if last_op != op {
            return Err(Error::Database(format!(
                "Last operation does not match -- cannot remove \
                 (expected {op:?}, got {last_op:?})"
            )));
        }

        execute_sql_stmt(t, &remove_operation_stmt(&last_id))?;
        Ok(())
    }

    async fn get_tag_color(&mut self, name: String) -> Result<Option<String>> {
        let t = self.get_txn()?;
        let color = t
            .query_row(TAG_COLOR_READ_SQL, [&name], |row| row.get::<_, String>(1))
            .optional()
            .context("Get tag color")?;
        Ok(color)
    }

    async fn set_tag_color(&mut self, name: String, color: String) -> Result<()> {
        let t = self.get_txn()?;
        let existing_id: Option<String> = t
            .query_row(TAG_COLOR_READ_SQL, [&name], |row| row.get::<_, String>(0))
            .optional()
            .context("Check existing tag color")?;
        let stmt = set_tag_color_stmt(&name, &color, &self.user_id, existing_id.as_deref());
        execute_sql_stmt(t, &stmt)?;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        let t = self
            .txn
            .take()
            .ok_or_else(|| Error::Database("Transaction already committed".into()))?;
        t.commit().context("Committing transaction")?;
        Ok(())
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn normal_undo_point() {
        // Normal: locally-written UndoPoint → "UndoPoint"
        let data = r#""UndoPoint""#;
        let op = parse_operation(data).unwrap();
        assert!(op.is_undo_point());
    }

    #[test]
    fn double_encoded_undo_point() {
        // Double-encoded: JSONB round-trip wraps the JSON string in another layer
        // Simulates: SQLite TEXT column contains "\"UndoPoint\""
        let data = r#""\"UndoPoint\"""#;
        let op = parse_operation(data).unwrap();
        assert!(op.is_undo_point());
    }

    #[test]
    fn double_encoded_invalid_variant() {
        // Double-encoded but inner value is not a valid Operation variant.
        // Unwrap succeeds, but second parse fails → should return Err, not panic.
        let data = r#""\"NotARealVariant\"""#;
        let result = parse_operation(data);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("unwrapped"),
            "error should indicate unwrapped path: {err_msg}"
        );
    }

    #[test]
    fn normal_create() {
        // Object variants are not double-encoded
        let uuid = uuid::Uuid::new_v4();
        let data = format!(r#"{{"Create":{{"uuid":"{}"}}}}"#, uuid);
        let op = parse_operation(&data).unwrap();
        assert_eq!(op.get_uuid(), Some(uuid));
    }

    #[test]
    fn invalid_data() {
        let data = "not valid json at all";
        assert!(parse_operation(data).is_err());
    }
}
