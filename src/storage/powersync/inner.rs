use crate::errors::{Error, Result};
use crate::operation::Operation;
use crate::storage::send_wrapper::{WrappedStorage, WrappedStorageTxn};
use crate::storage::{TaskMap, VersionId, DEFAULT_BASE_VERSION};
use anyhow::Context;
use async_trait::async_trait;
use chrono::DateTime;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::ffi::c_int;
use std::path::Path;
use std::sync::OnceLock;
use uuid::Uuid;

// The PowerSync C extension is statically linked via powersync_sqlite_nostd.
// We only need one entry point: powersync_init_static() registers the extension
// as a SQLite auto-extension so it fires on every subsequent Connection::open().
unsafe extern "C" {
    fn powersync_init_static() -> c_int;
}

static POWERSYNC_RC: OnceLock<c_int> = OnceLock::new();

/// Register the PowerSync SQLite auto-extension exactly once per process.
/// Returns Err if the extension registration fails (on every call, not just the first).
fn init_powersync_extension() -> Result<()> {
    // SAFETY: powersync_init_static calls sqlite3_auto_extension.
    // OnceLock guarantees the closure runs exactly once, so there is no concurrent call.
    let rc = POWERSYNC_RC.get_or_init(|| unsafe { powersync_init_static() });
    if *rc != 0 {
        return Err(Error::Database(format!(
            "Failed to load PowerSync extension (rc={rc})"
        )));
    }
    Ok(())
}

/// Convert Unix epoch string (e.g. "1724612771") to ISO 8601 string.
fn epoch_to_iso(epoch_str: &str) -> Option<String> {
    let secs: i64 = epoch_str.parse().ok()?;
    let dt = DateTime::from_timestamp(secs, 0)?;
    Some(dt.to_rfc3339())
}

/// Convert ISO 8601 string back to Unix epoch string.
fn iso_to_epoch(iso_str: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(iso_str).ok()?;
    Some(dt.timestamp().to_string())
}

/// Remove a timestamp field from `task_data`, converting epoch → ISO 8601.
/// Returns `Err` if the value is present but not a valid epoch integer.
fn extract_timestamp(task_data: &mut TaskMap, key: &str) -> Result<Option<String>> {
    match task_data.remove(key) {
        None => Ok(None),
        Some(epoch_str) => match epoch_to_iso(&epoch_str) {
            Some(iso) => Ok(Some(iso)),
            None => Err(Error::Database(format!(
                "Invalid epoch timestamp for field {key:?}: {epoch_str:?}"
            ))),
        },
    }
}

/// Raw row fetched from tc_tasks JOIN projects.
struct RawTaskRow {
    id: String,
    data: String,
    status: Option<String>,
    description: Option<String>,
    priority: Option<String>,
    entry_at: Option<String>,
    modified_at: Option<String>,
    due_at: Option<String>,
    scheduled_at: Option<String>,
    start_at: Option<String>,
    end_at: Option<String>,
    wait_at: Option<String>,
    parent_id: Option<String>,
    project_name: Option<String>,
}

fn read_raw_task_row(r: &rusqlite::Row) -> rusqlite::Result<RawTaskRow> {
    Ok(RawTaskRow {
        id: r.get("id")?,
        data: r.get("data")?,
        status: r.get("status")?,
        description: r.get("description")?,
        priority: r.get("priority")?,
        entry_at: r.get("entry_at")?,
        modified_at: r.get("modified_at")?,
        due_at: r.get("due_at")?,
        scheduled_at: r.get("scheduled_at")?,
        start_at: r.get("start_at")?,
        end_at: r.get("end_at")?,
        wait_at: r.get("wait_at")?,
        parent_id: r.get("parent_id")?,
        project_name: r.get("project_name")?,
    })
}

fn raw_to_task(raw: RawTaskRow) -> Result<(Uuid, TaskMap)> {
    let uuid = Uuid::parse_str(&raw.id)
        .map_err(|e| Error::Database(format!("Invalid UUID in tc_tasks: {e}")))?;
    let mut task_map: TaskMap = serde_json::from_str(&raw.data)
        .map_err(|e| Error::Database(format!("Failed to parse task data: {e}")))?;

    // Inject string columns back into the task map.
    if let Some(v) = raw.status {
        task_map.insert("status".into(), v);
    }
    if let Some(v) = raw.description {
        task_map.insert("description".into(), v);
    }
    if let Some(v) = raw.priority {
        task_map.insert("priority".into(), v);
    }
    if let Some(v) = raw.parent_id {
        task_map.insert("parent".into(), v);
    }
    if let Some(v) = raw.project_name {
        task_map.insert("project".into(), v);
    }

    // Inject timestamp columns (ISO 8601 → epoch string).
    // An ISO value present in the DB but failing to parse is a data integrity error.
    for (col_val, taskmap_key) in [
        (raw.entry_at, "entry"),
        (raw.modified_at, "modified"),
        (raw.due_at, "due"),
        (raw.scheduled_at, "scheduled"),
        (raw.start_at, "start"),
        (raw.end_at, "end"),
        (raw.wait_at, "wait"),
    ] {
        if let Some(iso) = col_val {
            let epoch = iso_to_epoch(&iso).ok_or_else(|| {
                Error::Database(format!(
                    "Malformed ISO timestamp in column {taskmap_key}_at: {iso:?}"
                ))
            })?;
            task_map.insert(taskmap_key.into(), epoch);
        }
    }

    Ok((uuid, task_map))
}

/// Shared column projection for all tc_tasks queries (requires `t` and `p` aliases).
const TASK_SELECT_COLS: &str = "t.id, t.data, t.status, t.description, t.priority, \
    t.entry_at, t.modified_at, t.due_at, t.scheduled_at, \
    t.start_at, t.end_at, t.wait_at, t.parent_id, p.name as project_name";

/// Execute a task SELECT query and convert each row to `(Uuid, TaskMap)`.
fn query_task_rows(
    t: &rusqlite::Transaction<'_>,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<(Uuid, TaskMap)>> {
    let mut q = t.prepare(sql)?;
    let rows: Vec<RawTaskRow> = q
        .query_map(params, read_raw_task_row)?
        .collect::<std::result::Result<_, _>>()?;
    rows.into_iter().map(raw_to_task).collect()
}

pub(super) struct PowerSyncStorageInner {
    conn: Connection,
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
    #[cfg(test)]
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
                project_id TEXT
            );
            CREATE TABLE IF NOT EXISTS tc_operations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT,
                data TEXT NOT NULL,
                created_at TEXT DEFAULT (datetime('now'))
            );
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                name TEXT,
                user_id TEXT,
                created_at TEXT DEFAULT (datetime('now'))
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

        let new_id = Uuid::new_v4().to_string();
        t.execute(
            "INSERT OR IGNORE INTO projects (id, name, user_id) VALUES (?, ?, ?)",
            params![&new_id, name, &self.user_id.to_string()],
        )?;

        // If INSERT was ignored (PK collision, astronomically unlikely with UUIDs),
        // re-query to get the authoritative ID rather than returning a dangling ref.
        if t.changes() == 0 {
            t.query_row(
                "SELECT id FROM projects WHERE name = ? ORDER BY created_at LIMIT 1",
                [name],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::Database(format!("Failed to resolve project id for {name:?}")))
        } else {
            Ok(new_id)
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
        raw_opt
            .map(|raw| raw_to_task(raw).map(|(_, task_map)| task_map))
            .transpose()
    }

    async fn get_pending_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {TASK_SELECT_COLS}
             FROM tc_working_set ws
             INNER JOIN tc_tasks t ON ws.uuid = t.id
             LEFT JOIN projects p ON t.project_id = p.id
             WHERE ws.uuid IS NOT NULL"
        );
        query_task_rows(t, &sql, [])
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
        let parent_id = task_data.remove("parent");

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

        let data_str = serde_json::to_string(&task_data)
            .map_err(|e| Error::Database(format!("Failed to serialize task data: {e}")))?;

        // Use upsert (INSERT ... ON CONFLICT DO UPDATE) rather than INSERT OR REPLACE.
        // INSERT OR REPLACE performs a DELETE + INSERT, which resets any columns not in
        // the INSERT list and may interfere with PowerSync's change-tracking triggers.
        let t = self.get_txn()?;
        t.execute(
            "INSERT INTO tc_tasks
             (id, user_id, data, status, description, priority,
              entry_at, modified_at, due_at, scheduled_at, start_at, end_at, wait_at,
              parent_id, project_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               user_id = excluded.user_id,
               data = excluded.data,
               status = excluded.status,
               description = excluded.description,
               priority = excluded.priority,
               entry_at = excluded.entry_at,
               modified_at = excluded.modified_at,
               due_at = excluded.due_at,
               scheduled_at = excluded.scheduled_at,
               start_at = excluded.start_at,
               end_at = excluded.end_at,
               wait_at = excluded.wait_at,
               parent_id = excluded.parent_id,
               project_id = excluded.project_id",
            params![
                &uuid.to_string(),
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
                &project_id,
            ],
        )
        .context("Set task query")?;
        Ok(())
    }

    async fn delete_task(&mut self, uuid: Uuid) -> Result<bool> {
        let t = self.get_txn()?;
        let changed = t
            .execute("DELETE FROM tc_tasks WHERE id = ?", [&uuid.to_string()])
            .context("Delete task query")?;
        Ok(changed > 0)
    }

    async fn all_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {TASK_SELECT_COLS}
             FROM tc_tasks t
             LEFT JOIN projects p ON t.project_id = p.id"
        );
        query_task_rows(t, &sql, [])
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

    async fn base_version(&mut self) -> Result<VersionId> {
        Ok(DEFAULT_BASE_VERSION)
    }

    async fn set_base_version(&mut self, _version: VersionId) -> Result<()> {
        Ok(())
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

    // PowerSync handles sync externally via flicknote-sync; these methods are no-ops.

    async fn unsynced_operations(&mut self) -> Result<Vec<Operation>> {
        Ok(vec![])
    }

    async fn num_unsynced_operations(&mut self) -> Result<usize> {
        Ok(0)
    }

    async fn add_operation(&mut self, op: Operation) -> Result<()> {
        let data_str = serde_json::to_string(&op)
            .map_err(|e| Error::Database(format!("Failed to serialize operation: {e}")))?;
        let t = self.get_txn()?;
        t.execute(
            "INSERT INTO tc_operations (user_id, data) VALUES (?, ?)",
            params![&self.user_id.to_string(), &data_str],
        )
        .context("Add operation query")?;
        Ok(())
    }

    async fn remove_operation(&mut self, op: Operation) -> Result<()> {
        let t = self.get_txn()?;
        let last: Option<(i64, String)> = t
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
        t.execute("DELETE FROM tc_operations WHERE id = ?", [last_id])
            .context("Remove operation")?;
        Ok(())
    }

    async fn sync_complete(&mut self) -> Result<()> {
        Ok(())
    }

    // Working set is not used with PowerSync; task numbering is not meaningful here.

    async fn get_working_set(&mut self) -> Result<Vec<Option<Uuid>>> {
        Ok(vec![])
    }

    async fn add_to_working_set(&mut self, _uuid: Uuid) -> Result<usize> {
        Ok(0)
    }

    async fn set_working_set_item(&mut self, _index: usize, _uuid: Option<Uuid>) -> Result<()> {
        Ok(())
    }

    async fn clear_working_set(&mut self) -> Result<()> {
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
