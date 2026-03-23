use crate::errors::{Error, Result};
use crate::operation::Operation;
use crate::storage::send_wrapper::{WrappedStorage, WrappedStorageTxn};
use crate::storage::TaskMap;
use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;
use uuid::Uuid;

use super::columns::{
    extract_timestamp, query_task_rows, raw_to_task, read_raw_task_row, TASK_SELECT_COLS,
};
use super::extension::init_powersync_extension;

const TS_FMT: &str = "%Y-%m-%d %H:%M:%S%.3f";

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

pub(super) struct PowerSyncStorageInner {
    pub(super) conn: Connection,
    user_id: Uuid,
}

/// Newtype wrapper that makes a raw SQLite pointer `Send`.
///
/// Sound because the Wrapper actor guarantees single-threaded access —
/// only one thread touches the connection at a time.
pub(super) struct SendPtr(pub(super) *mut rusqlite::ffi::sqlite3);
unsafe impl Send for SendPtr {}

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

    /// Create a `PowerSyncStorageInner` from an existing SQLite handle.
    ///
    /// # Safety
    ///
    /// - `ptr.0` must be a valid, open `sqlite3*` pointer
    /// - The caller retains ownership — this connection will NOT close the handle on drop
    /// - The handle must remain valid for the lifetime of this `PowerSyncStorageInner`
    /// - PowerSync SDK must have already initialized the connection (WAL mode, powersync_init,
    ///   tc_tasks view, etc.)
    pub(super) unsafe fn from_handle(ptr: SendPtr, user_id: Uuid) -> Result<Self> {
        let conn = Connection::from_handle(ptr.0)
            .context("Creating rusqlite Connection from raw handle")?;
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
        t.execute(
            "INSERT INTO tc_tasks (id, user_id, data) VALUES (?, ?, '{}')",
            params![&uuid.to_string(), &self.user_id.to_string()],
        )
        .context("Create task query")?;
        Ok(true)
    }

    async fn set_task(&mut self, uuid: Uuid, task: TaskMap) -> Result<()> {
        let mut task_data = task;

        // Extract string columns.
        let status = task_data.remove("status");
        let description = task_data.remove("description");
        let priority = task_data.remove("priority");
        let parent_id = task_data.remove("parent_id");
        let position = task_data.remove("position");

        // Extract and convert timestamp columns. An Err propagates immediately,
        // aborting set_task before any DB write, so no partial state is committed.
        let entry_at = extract_timestamp(&mut task_data, "entry")?;
        let modified_at = extract_timestamp(&mut task_data, "modified")?;
        let due_at = extract_timestamp(&mut task_data, "due")?;
        let scheduled_at = extract_timestamp(&mut task_data, "scheduled")?;
        let start_at = extract_timestamp(&mut task_data, "start")?;
        let end_at = extract_timestamp(&mut task_data, "end")?;
        let wait_at = extract_timestamp(&mut task_data, "wait")?;

        // Resolve project name → project_id (look up or create in projects table).
        let project_id: Option<String> = task_data
            .remove("project")
            .map(|name| self.resolve_project_id(&name))
            .transpose()?;

        // Extract tags (tag_<name> → "") before serializing data blob.
        // Collect keys first to break the immutable borrow, then remove via mutable borrow.
        let tag_names: Vec<String> = task_data
            .keys()
            .filter(|k| k.starts_with("tag_"))
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|k| {
                task_data.remove(&k);
                k.strip_prefix("tag_").map(String::from)
            })
            .collect();

        // Extract annotations (annotation_<epoch> → description) before serializing data blob.
        // Always remove annotation keys from task_data (even on error), then validate the epoch.
        // An unparseable suffix is a data-integrity error — propagate rather than leave a
        // stale key in the blob.
        let annotation_keys: Vec<String> = task_data
            .keys()
            .filter(|k| k.starts_with("annotation_"))
            .cloned()
            .collect();
        let annotations: Vec<(i64, String)> = annotation_keys
            .into_iter()
            .map(|k| {
                let desc = task_data.remove(&k).expect("key collected from same map");
                let epoch_str = k.strip_prefix("annotation_").unwrap();
                let epoch: i64 = epoch_str.parse().map_err(|_| {
                    Error::Database(format!(
                        "Invalid annotation key {k:?}: epoch suffix is not an integer"
                    ))
                })?;
                Ok((epoch, desc))
            })
            .collect::<Result<Vec<_>>>()?;

        let data_str = serde_json::to_string(&task_data)
            .map_err(|e| Error::Database(format!("Failed to serialize task data: {e}")))?;

        // PowerSync views don't support UPSERT (INSERT ... ON CONFLICT DO UPDATE).
        // INSTEAD OF triggers also report 0 rows changed regardless of success,
        // so we check existence with SELECT, then INSERT or UPDATE accordingly.
        let t = self.get_txn()?;
        let uuid_str = uuid.to_string();
        let exists: bool = t
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tc_tasks WHERE id = ?)",
                [&uuid_str],
                |row| row.get(0),
            )
            .context("Set task existence check")?;

        if exists {
            t.execute(
                "UPDATE tc_tasks SET
                   user_id = ?, data = ?, status = ?, description = ?, priority = ?,
                   entry_at = ?, modified_at = ?, due_at = ?, scheduled_at = ?,
                   start_at = ?, end_at = ?, wait_at = ?, parent_id = ?, position = ?, project_id = ?
                 WHERE id = ?",
                params![
                    &self.user_id.to_string(),
                    &data_str,
                    &status,
                    &description,
                    &priority,
                    &entry_at,
                    &modified_at,
                    &due_at,
                    &scheduled_at,
                    &start_at,
                    &end_at,
                    &wait_at,
                    &parent_id,
                    &position,
                    &project_id,
                    &uuid_str,
                ],
            )
            .context("Set task UPDATE query")?;
        } else {
            t.execute(
                "INSERT INTO tc_tasks
                 (id, user_id, data, status, description, priority,
                  entry_at, modified_at, due_at, scheduled_at, start_at, end_at, wait_at,
                  parent_id, position, project_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    &uuid_str,
                    &self.user_id.to_string(),
                    &data_str,
                    &status,
                    &description,
                    &priority,
                    &entry_at,
                    &modified_at,
                    &due_at,
                    &scheduled_at,
                    &start_at,
                    &end_at,
                    &wait_at,
                    &parent_id,
                    &position,
                    &project_id,
                ],
            )
            .context("Set task INSERT query")?;
        }

        // Sync tags: delete all existing rows for this task, then insert current set.
        let user_id_str = self.user_id.to_string();
        t.execute("DELETE FROM tc_tags WHERE task_id = ?", [&uuid_str])
            .context("Delete existing tags")?;
        for tag_name in &tag_names {
            t.execute(
                "INSERT INTO tc_tags (id, task_id, user_id, name) VALUES (?, ?, ?, ?)",
                params![
                    &Uuid::now_v7().to_string(),
                    &uuid_str,
                    &user_id_str,
                    tag_name,
                ],
            )
            .context("Insert tag")?;
        }

        // Sync annotations: delete all existing rows for this task, then insert current set.
        t.execute("DELETE FROM tc_annotations WHERE task_id = ?", [&uuid_str])
            .context("Delete existing annotations")?;
        for (epoch, description) in &annotations {
            let entry_at = DateTime::from_timestamp(*epoch, 0)
                .map(|dt| dt.to_rfc3339())
                .ok_or_else(|| Error::Database(format!("Invalid annotation timestamp: {epoch}")))?;
            t.execute(
                "INSERT INTO tc_annotations (id, task_id, user_id, entry_at, description) VALUES (?, ?, ?, ?, ?)",
                params![
                    &Uuid::now_v7().to_string(),
                    &uuid_str,
                    &user_id_str,
                    &entry_at,
                    description,
                ],
            )
            .context("Insert annotation")?;
        }

        Ok(())
    }

    async fn delete_task(&mut self, uuid: Uuid) -> Result<bool> {
        let t = self.get_txn()?;
        let uuid_str = uuid.to_string();
        // INSTEAD OF triggers on PowerSync views report 0 rows changed,
        // so check existence before DELETE to return the correct boolean.
        let exists: bool = t
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tc_tasks WHERE id = ?)",
                [&uuid_str],
                |row| row.get(0),
            )
            .context("Delete task existence check")?;
        if exists {
            t.execute("DELETE FROM tc_tags WHERE task_id = ?", [&uuid_str])
                .context("Delete task tags")?;
            t.execute("DELETE FROM tc_annotations WHERE task_id = ?", [&uuid_str])
                .context("Delete task annotations")?;
            t.execute("DELETE FROM tc_tasks WHERE id = ?", [&uuid_str])
                .context("Delete task query")?;
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
        let mut q = t.prepare("SELECT id FROM tc_tasks")?;
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
        let mut q = t.prepare("SELECT data FROM tc_operations ORDER BY id ASC")?;
        let rows = q.query_map([], |r| r.get::<_, String>("data"))?;
        let raw: Vec<String> = rows.collect::<std::result::Result<_, _>>()?;
        raw.into_iter()
            .map(|data_str| {
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

    // PowerSync handles sync externally; unsynced_operations returns empty.
    async fn unsynced_operations(&mut self) -> Result<Vec<Operation>> {
        Ok(vec![])
    }

    async fn add_operation(&mut self, op: Operation) -> Result<()> {
        // Only Update carries a timestamp; Create, Delete, and UndoPoint don't have one.
        let created_at = match &op {
            Operation::Update { timestamp, .. } => timestamp.format(TS_FMT).to_string(),
            Operation::Create { .. } | Operation::Delete { .. } | Operation::UndoPoint => {
                Utc::now().format(TS_FMT).to_string()
            }
        };
        let data_str = serde_json::to_string(&op)
            .map_err(|e| Error::Database(format!("Failed to serialize operation: {e}")))?;
        let t = self.get_txn()?;
        t.execute(
            "INSERT INTO tc_operations (id, user_id, data, created_at) VALUES (?, ?, ?, ?)",
            params![
                &Uuid::now_v7().to_string(),
                &self.user_id.to_string(),
                &data_str,
                &created_at
            ],
        )
        .context("Add operation query")?;
        Ok(())
    }

    async fn remove_operation(&mut self, op: Operation) -> Result<()> {
        let t = self.get_txn()?;
        let last: Option<(String, String)> = t
            .query_row(
                "SELECT id, data FROM tc_operations ORDER BY id DESC LIMIT 1",
                [],
                |x| Ok((x.get(0)?, x.get(1)?)),
            )
            .optional()?;

        let Some((last_id, last_data)) = last else {
            return Err(Error::Database("No operations to remove".into()));
        };

        let last_op: Operation = serde_json::from_str(&last_data)
            .map_err(|e| Error::Database(format!("Failed to parse operation: {e}")))?;

        if last_op != op {
            return Err(Error::Database(format!(
                "Last operation does not match -- cannot remove \
                 (expected {op:?}, got {last_op:?})"
            )));
        }

        let t = self.get_txn()?;
        t.execute("DELETE FROM tc_operations WHERE id = ?", [&last_id])
            .context("Remove operation")?;
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
