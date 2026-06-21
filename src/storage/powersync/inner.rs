use crate::errors::{Error, Result};
use crate::operation::Operation;
use crate::storage::TaskMap;
use anyhow::Context;
use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Pool, Sqlite, Transaction};
use std::path::Path;
use uuid::Uuid;

use super::extension::init_powersync_extension;
use crate::storage::columns::raw_to_task;
use crate::storage::columns::RawTaskRow;
use crate::storage::columns::TASK_SELECT_COLS;
use crate::storage::sql_ops::{
    add_operation_stmt, create_task_stmt, delete_task_stmts, prepare_task, remove_operation_stmt,
    set_task_stmts, set_tc_config_stmt, SqlStatement, ALL_OPERATIONS_SQL, ALL_TAGS_SQL,
    ALL_TASK_UUIDS_SQL, LAST_OPERATION_SQL, TASK_EXISTS_SQL, TC_CONFIG_READ_SQL,
};

/// Execute a SqlStatement against a sqlx Sqlite transaction.
async fn execute_sql_stmt(t: &mut Transaction<'_, Sqlite>, stmt: &SqlStatement) -> Result<()> {
    let mut query = sqlx::query(&stmt.sql);
    for param in &stmt.params {
        match param {
            crate::storage::sql_ops::SqlParam::Text(s) => query = query.bind(s),
            crate::storage::sql_ops::SqlParam::Null => query = query.bind(Option::<String>::None),
        }
    }
    query
        .execute(&mut **t)
        .await
        .context("Executing SQL statement")?;
    Ok(())
}

pub(super) struct PowerSyncStorageInner {
    pub(super) pool: Pool<Sqlite>,
}

impl PowerSyncStorageInner {
    /// Open an existing PowerSync-managed database file and create local-only tables.
    pub(super) async fn new(db_path: &Path) -> Result<Self> {
        // Register the PowerSync extension as a SQLite auto-extension (once per process).
        init_powersync_extension()?;

        let db_path = db_path.to_path_buf();
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .pragma("busy_timeout", "30000");

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .context("Opening PowerSync database")?;

        // Verify the DB has been initialized by flicknote-sync (tc_tasks view must exist).
        let has_tc_tasks: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='view' AND name='tc_tasks'",
        )
        .fetch_one(&pool)
        .await
        .context("Checking for tc_tasks view")?;
        if !has_tc_tasks {
            return Err(Error::Database(
                "tc_tasks view not found — the database must be initialized by flicknote-sync \
                 before flicktask can use it. Run flicknote-sync first to set up PowerSync views."
                    .into(),
            ));
        }

        // Initialize PowerSync internal tables (ps_migration, ps_oplog, etc.).
        // This does NOT create user-facing views — those already exist from flicknote-sync.
        // We intentionally do NOT call powersync_replace_schema here because it performs
        // a FULL REPLACE — it would drop views for notes, projects, note_extractions
        // that flicknote-sync registered. We only need the extension functions loaded
        // (which happened at Connection::open via auto-extension).
        sqlx::query("SELECT powersync_init()")
            .execute(&pool)
            .await
            .context("PowerSync init")?;

        Ok(Self { pool })
    }

    /// Create an in-memory database with all required tables for testing.
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) async fn new_for_test() -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .context("Creating in-memory database")?;

        sqlx::query(
            "
            CREATE TABLE IF NOT EXISTS tc_tasks (
                id TEXT PRIMARY KEY,
                short_id INTEGER,
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
                project_id TEXT,
                note_id TEXT
            );
            CREATE TABLE IF NOT EXISTS tc_operations (
                id TEXT PRIMARY KEY,
                data TEXT NOT NULL,
                created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
            );
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                name TEXT,
                created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
            );
            CREATE TABLE IF NOT EXISTS tc_tag_metadata (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                data TEXT NOT NULL DEFAULT '{}',
                created_at TEXT DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now'))
            );
            CREATE TABLE IF NOT EXISTS settings (
                id TEXT PRIMARY KEY,
                tc_config TEXT
            );
            INSERT INTO settings (id) VALUES ('default') ON CONFLICT(id) DO NOTHING;
        ",
        )
        .execute(&pool)
        .await
        .context("Creating PowerSync test tables")?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl crate::storage::Storage for PowerSyncStorageInner {
    async fn txn<'a>(&'a mut self) -> Result<Box<dyn crate::storage::StorageTxn + Send + 'a>> {
        let txn = self.pool.begin().await.context("Starting transaction")?;
        Ok(Box::new(PowerSyncTxn { txn: Some(txn) }))
    }
}

pub(super) struct PowerSyncTxn<'a> {
    txn: Option<Transaction<'a, Sqlite>>,
}

impl<'a> PowerSyncTxn<'a> {
    fn get_txn(&mut self) -> Result<&mut Transaction<'a, Sqlite>> {
        self.txn
            .as_mut()
            .ok_or_else(|| Error::Database("Transaction already committed".into()))
    }

    /// Resolve a project name to its UUID via the projects table.
    async fn resolve_project_id(&mut self, name: &str) -> Result<String> {
        let t = self.get_txn()?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT id FROM projects WHERE name = ? ORDER BY created_at LIMIT 1")
                .bind(name)
                .fetch_optional(&mut **t)
                .await
                .context("Resolving project id")?;
        match row {
            Some((id,)) => Ok(id),
            None => Err(Error::ProjectNotFound(name.to_string())),
        }
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

#[async_trait]
impl crate::storage::StorageTxn for PowerSyncTxn<'_> {
    async fn get_task(&mut self, uuid: Uuid) -> Result<Option<TaskMap>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {TASK_SELECT_COLS}
             FROM tc_tasks t
             LEFT JOIN projects p ON t.project_id = p.id
             WHERE t.id = ? LIMIT 1"
        );
        let row: Option<RawTaskRow> = sqlx::query_as(&sql)
            .bind(uuid.to_string())
            .fetch_optional(&mut **t)
            .await
            .context("get_task query")?;

        match row {
            None => Ok(None),
            Some(raw) => {
                let (_, task_map) = raw_to_task(raw)?;
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
        let rows: Vec<RawTaskRow> = sqlx::query_as(&sql)
            .fetch_all(&mut **t)
            .await
            .context("get_pending_tasks query")?;

        rows.into_iter().map(raw_to_task).collect()
    }

    async fn create_task(&mut self, uuid: Uuid) -> Result<bool> {
        let t = self.get_txn()?;
        let count: (i64,) = sqlx::query_as("SELECT count(id) FROM tc_tasks WHERE id = ?")
            .bind(uuid.to_string())
            .fetch_one(&mut **t)
            .await
            .context("create_task count")?;
        if count.0 > 0 {
            return Ok(false);
        }
        execute_sql_stmt(t, &create_task_stmt(&uuid)).await?;
        Ok(true)
    }

    async fn set_task(&mut self, uuid: Uuid, task: TaskMap) -> Result<()> {
        let prepared = prepare_task(task)?;

        // Resolve project name first. If a project name is provided and cannot be resolved,
        // the error propagates immediately — there is no fallback to raw UUID.
        // If no name is provided, fall back to the raw UUID if present.
        let project_id = if let Some(name) = &prepared.project_name {
            match self.resolve_project_id(name).await {
                Ok(id) => Some(id),
                Err(e) => return Err(e),
            }
        } else {
            prepared.project_id_raw.clone()
        };

        // PowerSync views don't support UPSERT (INSERT ... ON CONFLICT DO UPDATE).
        // INSTEAD OF triggers also report 0 rows changed regardless of success,
        // so we check existence with SELECT, then INSERT or UPDATE accordingly.
        let t = self.get_txn()?;
        let exists: (bool,) = sqlx::query_as(TASK_EXISTS_SQL)
            .bind(uuid.to_string())
            .fetch_one(&mut **t)
            .await
            .context("Set task existence check")?;

        // Generate and execute statements.
        let stmts = set_task_stmts(&uuid, &prepared, exists.0, project_id.as_deref())?;
        for stmt in &stmts {
            execute_sql_stmt(t, stmt).await?;
        }
        Ok(())
    }

    async fn delete_task(&mut self, uuid: Uuid) -> Result<bool> {
        let t = self.get_txn()?;
        let uuid_str = uuid.to_string();
        // INSTEAD OF triggers on PowerSync views report 0 rows changed,
        // so check existence before DELETE to return the correct boolean.
        let exists: (bool,) = sqlx::query_as(TASK_EXISTS_SQL)
            .bind(&uuid_str)
            .fetch_one(&mut **t)
            .await
            .context("Delete task existence check")?;
        if exists.0 {
            for stmt in &delete_task_stmts(&uuid) {
                execute_sql_stmt(t, stmt).await?;
            }
        }
        Ok(exists.0)
    }

    async fn all_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {TASK_SELECT_COLS}
             FROM tc_tasks t
             LEFT JOIN projects p ON t.project_id = p.id"
        );
        let rows: Vec<RawTaskRow> = sqlx::query_as(&sql)
            .fetch_all(&mut **t)
            .await
            .context("all_tasks query")?;

        rows.into_iter().map(raw_to_task).collect()
    }

    async fn all_task_uuids(&mut self) -> Result<Vec<Uuid>> {
        let t = self.get_txn()?;
        let rows: Vec<(String,)> = sqlx::query_as(ALL_TASK_UUIDS_SQL)
            .fetch_all(&mut **t)
            .await
            .context("all_task_uuids query")?;

        rows.into_iter()
            .map(|(s,)| {
                Uuid::parse_str(&s).map_err(|e| Error::Database(format!("Invalid UUID: {e}")))
            })
            .collect()
    }

    async fn get_task_operations(&mut self, uuid: Uuid) -> Result<Vec<Operation>> {
        // tc_operations has no UUID column (schema is PowerSync-managed).
        // Filter in memory after deserializing; acceptable for the expected operation count.
        let t = self.get_txn()?;
        let rows: Vec<(String,)> = sqlx::query_as(ALL_OPERATIONS_SQL)
            .fetch_all(&mut **t)
            .await
            .context("get_task_operations query")?;

        let mut ops = Vec::new();
        for (data_str,) in rows {
            let op = parse_operation(&data_str)?;
            if op.get_uuid() == Some(uuid) {
                ops.push(op);
            }
        }
        Ok(ops)
    }

    async fn all_operations(&mut self) -> Result<Vec<Operation>> {
        let t = self.get_txn()?;
        let rows: Vec<(String,)> = sqlx::query_as(ALL_OPERATIONS_SQL)
            .fetch_all(&mut **t)
            .await
            .context("all_operations query")?;

        rows.into_iter()
            .map(|(data_str,)| parse_operation(&data_str))
            .collect()
    }

    async fn add_operation(&mut self, op: Operation) -> Result<()> {
        let t = self.get_txn()?;
        execute_sql_stmt(t, &add_operation_stmt(&op)?).await?;
        Ok(())
    }

    async fn remove_operation(&mut self, op: Operation) -> Result<()> {
        let t = self.get_txn()?;
        let last: Option<(String, String)> = sqlx::query_as(LAST_OPERATION_SQL)
            .fetch_optional(&mut **t)
            .await
            .context("remove_operation: fetch last")?;

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

        execute_sql_stmt(t, &remove_operation_stmt(&last_id)).await?;
        Ok(())
    }

    async fn get_all_tags(&mut self) -> Result<Vec<String>> {
        let t = self.get_txn()?;
        let rows: Vec<(String,)> = sqlx::query_as(ALL_TAGS_SQL)
            .fetch_all(&mut **t)
            .await
            .context("get_all_tags query")?;

        Ok(rows.into_iter().map(|(name,)| name).collect())
    }

    async fn get_tc_config(&mut self) -> Result<Option<String>> {
        let t = self.get_txn()?;
        let row: Option<(Option<String>,)> = sqlx::query_as(TC_CONFIG_READ_SQL)
            .fetch_optional(&mut **t)
            .await
            .context("get_tc_config query")?;

        Ok(row.and_then(|(v,)| v))
    }

    async fn set_tc_config(&mut self, value: String) -> Result<()> {
        let t = self.get_txn()?;
        execute_sql_stmt(t, &set_tc_config_stmt(&value)).await
    }

    async fn commit(&mut self) -> Result<()> {
        let t = self
            .txn
            .take()
            .ok_or_else(|| Error::Database("Transaction already committed".into()))?;
        t.commit().await.context("Committing transaction")?;
        Ok(())
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn normal_undo_point() {
        // Normal: locally-written UndoPoint -> "UndoPoint"
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
        // Unwrap succeeds, but second parse fails -> should return Err, not panic.
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
