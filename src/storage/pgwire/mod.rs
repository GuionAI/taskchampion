//! Postgres storage backend via pgwire.
//!
//! Connects to `pgwire-supabase-proxy` using a Supabase JWT for authentication.
//! Uses `sqlx_core::Transaction` for real Postgres transactions.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx_core::connection::Connection;
use sqlx_core::executor::Executor;
use sqlx_core::query::query;
use sqlx_core::query_as::query_as;
use sqlx_core::query_scalar::query_scalar;
use sqlx_core::transaction::Transaction;
use sqlx_core::types::Json;
use sqlx_postgres::{PgConnection, Postgres};
use uuid::Uuid;

use crate::errors::{Error, Result};
use crate::operation::Operation;
use crate::storage::columns::raw_to_task;
use crate::storage::sql_ops::prepare_task;
use crate::storage::{Storage, StorageTxn, TaskMap};

mod row;
mod row_reader;
use row::{SettingsPgRow, TaskPgRow};
use row_reader::rows_to_tasks;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Convert an ISO8601 string from PreparedTask into a chrono DateTime<Utc>.
/// Returns None for None input. Errors on a non-empty string that fails to parse.
fn iso_to_datetime_utc(s: &Option<String>) -> Result<Option<DateTime<Utc>>> {
    match s {
        None => Ok(None),
        Some(iso) => DateTime::parse_from_rfc3339(iso)
            .map(|dt| Some(dt.with_timezone(&Utc)))
            .map_err(|e| {
                Error::Database(format!(
                    "invalid ISO timestamp from prepare_task ({iso:?}): {e}"
                ))
            }),
    }
}

/// Convert an Option<String> UUID to Option<Uuid>.
fn opt_str_to_uuid(s: &Option<String>) -> Result<Option<Uuid>> {
    match s {
        None => Ok(None),
        Some(v) => Uuid::parse_str(v).map(Some).map_err(|e| {
            Error::Database(format!(
                "invalid UUID string from prepare_task ({v:?}): {e}"
            ))
        }),
    }
}

// ── PgWireStorage ──────────────────────────────────────────────────────────

/// Postgres-backed storage that connects via the pgwire-supabase-proxy.
///
/// The proxy validates Supabase JWTs per-connection and sets RLS context.
/// Pass `DATABASE_URL` (full postgres:// URL with JWT as password) and `FLICKNOTE_TOKEN`
/// (Supabase JWT, for user_id extraction from JWT sub claim) to [`PgWireStorage::new`].
///
/// Connection URL must include `sslmode=disable` — pgwire-supabase-proxy runs over plain TCP.
pub struct PgWireStorage {
    conn: PgConnection,
}

impl PgWireStorage {
    /// Connect to Postgres via pgwire.
    ///
    /// - `database_url`: Postgres connection string, e.g.
    ///   `postgres://user:password@host:port/dbname?sslmode=disable`.
    ///   The JWT should be embedded as the password in the URL (the caller constructs this).
    /// - `token`: Supabase JWT. Kept for backwards compatibility — this parameter is no longer
    ///   used internally. The caller may use this token for user_id extraction (JWT sub claim).
    pub async fn new(database_url: &str, _token: &str) -> Result<Self> {
        let conn = PgConnection::connect(database_url).await?;
        Ok(Self { conn })
    }
}

#[async_trait]
impl Storage for PgWireStorage {
    async fn txn<'a>(&'a mut self) -> Result<Box<dyn StorageTxn + Send + 'a>> {
        let txn = self.conn.begin().await?;
        Ok(Box::new(PgWireTxn { txn: Some(txn) }))
    }
}

// ── PgWireTxn ─────────────────────────────────────────────────────────────

pub(super) struct PgWireTxn<'a> {
    txn: Option<Transaction<'a, Postgres>>,
}

impl<'a> PgWireTxn<'a> {
    fn get_txn(&mut self) -> Result<&mut Transaction<'a, Postgres>> {
        self.txn
            .as_mut()
            .ok_or_else(|| Error::Database("Transaction already committed".into()))
    }

    /// Check if a task with the given UUID exists.
    async fn task_exists(exec: impl Executor<'_, Database = Postgres>, uuid: Uuid) -> Result<bool> {
        let exists: bool = query_scalar("SELECT EXISTS(SELECT 1 FROM tc_tasks WHERE id = $1)")
            .bind(uuid)
            .fetch_one(exec)
            .await?;
        Ok(exists)
    }

    /// Resolve a project name to its UUID via the projects table.
    async fn resolve_project_id(
        exec: impl Executor<'_, Database = Postgres>,
        name: &str,
    ) -> Result<String> {
        let row: Option<(Uuid,)> =
            query_as("SELECT id FROM projects WHERE name = $1 ORDER BY created_at ASC LIMIT 1")
                .bind(name)
                .fetch_optional(exec)
                .await?;
        match row {
            Some((id,)) => Ok(id.to_string()),
            None => Err(Error::ProjectNotFound(name.to_string())),
        }
    }
}

impl Drop for PgWireTxn<'_> {
    fn drop(&mut self) {
        if self.txn.is_some() {
            log::debug!("PgWireTxn dropped without commit — implicit rollback");
        }
    }
}

#[async_trait]
impl<'a> StorageTxn for PgWireTxn<'a> {
    async fn get_task(&mut self, uuid: Uuid) -> Result<Option<TaskMap>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {} FROM tc_tasks t LEFT JOIN projects p ON t.project_id = p.id WHERE t.id = $1 LIMIT 1",
            crate::storage::columns::TASK_SELECT_COLS
        );
        let rows: Vec<TaskPgRow> = query_as::<_, TaskPgRow>(&sql)
            .bind(uuid)
            .fetch_all(&mut **t)
            .await?;
        match rows.into_iter().next() {
            None => Ok(None),
            Some(row) => {
                log::debug!("pgwire: get_task deserializing {uuid}");
                let raw = row.into();
                let (_, task_map) = raw_to_task(raw)?;
                Ok(Some(task_map))
            }
        }
    }

    async fn get_pending_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {} FROM tc_tasks t LEFT JOIN projects p ON t.project_id = p.id WHERE t.status = 'pending'",
            crate::storage::columns::TASK_SELECT_COLS
        );
        let rows: Vec<TaskPgRow> = query_as::<_, TaskPgRow>(&sql).fetch_all(&mut **t).await?;
        rows_to_tasks(rows)
    }

    async fn create_task(&mut self, uuid: Uuid) -> Result<bool> {
        if Self::task_exists(&mut **self.get_txn()?, uuid).await? {
            return Ok(false);
        }
        let t = self.get_txn()?;
        query("INSERT INTO tc_tasks (id, data) VALUES ($1, '{}')")
            .bind(uuid)
            .execute(&mut **t)
            .await?;
        Ok(true)
    }

    async fn set_task(&mut self, uuid: Uuid, mut task: TaskMap) -> Result<()> {
        let note_id_str = task.remove("note_id");
        let prepared = prepare_task(task)?;

        let t = self.get_txn()?;
        let t_ref = &mut **t;

        let project_id_str = if let Some(name) = &prepared.project_name {
            Some(Self::resolve_project_id(&mut *t_ref, name).await?)
        } else {
            prepared.project_id_raw.clone()
        };
        let project_id = opt_str_to_uuid(&project_id_str)?;
        let parent_id = opt_str_to_uuid(&prepared.parent_id)?;
        let note_id = opt_str_to_uuid(&note_id_str)?;

        let entry_at = iso_to_datetime_utc(&prepared.entry_at)?;
        let modified_at = iso_to_datetime_utc(&prepared.modified_at)?;
        let due_at = iso_to_datetime_utc(&prepared.due_at)?;
        let scheduled_at = iso_to_datetime_utc(&prepared.scheduled_at)?;
        let start_at = iso_to_datetime_utc(&prepared.start_at)?;
        let end_at = iso_to_datetime_utc(&prepared.end_at)?;
        let wait_at = iso_to_datetime_utc(&prepared.wait_at)?;

        let data_val: serde_json::Value = serde_json::from_str(&prepared.data_json)
            .map_err(|e| Error::Database(format!("set_task parse data: {e}")))?;

        if Self::task_exists(&mut *t_ref, uuid).await? {
            query(
                "UPDATE tc_tasks SET data = $1, status = $2, description = $3, \
                 priority = $4, entry_at = $5, modified_at = $6, due_at = $7, \
                 scheduled_at = $8, start_at = $9, end_at = $10, wait_at = $11, \
                 parent_id = $12, project_id = $13, note_id = $14 WHERE id = $15",
            )
            .bind(&data_val)
            .bind(&prepared.status)
            .bind(&prepared.description)
            .bind(&prepared.priority)
            .bind(entry_at)
            .bind(modified_at)
            .bind(due_at)
            .bind(scheduled_at)
            .bind(start_at)
            .bind(end_at)
            .bind(wait_at)
            .bind(parent_id)
            .bind(project_id)
            .bind(note_id)
            .bind(uuid)
            .execute(&mut *t_ref)
            .await?;
        } else {
            query(
                "INSERT INTO tc_tasks (id, data, status, description, priority, \
                 entry_at, modified_at, due_at, scheduled_at, start_at, end_at, \
                 wait_at, parent_id, project_id, note_id) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
            )
            .bind(uuid)
            .bind(&data_val)
            .bind(&prepared.status)
            .bind(&prepared.description)
            .bind(&prepared.priority)
            .bind(entry_at)
            .bind(modified_at)
            .bind(due_at)
            .bind(scheduled_at)
            .bind(start_at)
            .bind(end_at)
            .bind(wait_at)
            .bind(parent_id)
            .bind(project_id)
            .bind(note_id)
            .execute(&mut *t_ref)
            .await?;
        }
        Ok(())
    }

    async fn delete_task(&mut self, uuid: Uuid) -> Result<bool> {
        let t = self.get_txn()?;
        if !Self::task_exists(&mut **t, uuid).await? {
            return Ok(false);
        }
        let n = query::<Postgres>("DELETE FROM tc_tasks WHERE id = $1")
            .bind(uuid)
            .execute(&mut **t)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    async fn all_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {} FROM tc_tasks t LEFT JOIN projects p ON t.project_id = p.id",
            crate::storage::columns::TASK_SELECT_COLS
        );
        let rows: Vec<TaskPgRow> = query_as::<_, TaskPgRow>(&sql).fetch_all(&mut **t).await?;
        rows_to_tasks(rows)
    }

    async fn all_task_uuids(&mut self) -> Result<Vec<Uuid>> {
        let t = self.get_txn()?;
        let ids: Vec<Uuid> = query_scalar("SELECT id FROM tc_tasks")
            .fetch_all(&mut **t)
            .await?;
        Ok(ids)
    }

    async fn get_task_operations(&mut self, _uuid: Uuid) -> Result<Vec<Operation>> {
        Ok(vec![])
    }

    async fn all_operations(&mut self) -> Result<Vec<Operation>> {
        Err(Error::Database(
            "all_operations is not supported on the pgwire backend — \
             the remote Postgres is the source of truth and has no operation log. \
             Use a local backend (powersync / external / inmemory) for undo/replay."
                .into(),
        ))
    }

    async fn add_operation(&mut self, _op: Operation) -> Result<()> {
        log::debug!("pgwire add_operation: ignored (operation log not persisted on this backend)");
        Ok(())
    }

    async fn remove_operation(&mut self, _op: Operation) -> Result<()> {
        Err(Error::Database(
            "remove_operation is not supported on the pgwire backend — \
             the remote Postgres has no operation log; undo is unavailable on this backend."
                .into(),
        ))
    }

    async fn get_all_tags(&mut self) -> Result<Vec<String>> {
        let t = self.get_txn()?;
        let rows: Vec<(String,)> = query_as(
            "SELECT DISTINCT kv.key AS name \
             FROM tc_tasks, jsonb_each_text(data) AS kv \
             WHERE kv.key LIKE 'tag_%' \
             ORDER BY name",
        )
        .fetch_all(&mut **t)
        .await?;
        rows.into_iter()
            .map(|(key,)| Ok(key.strip_prefix("tag_").unwrap_or(&key).to_string()))
            .collect()
    }

    async fn get_tc_config(&mut self) -> Result<Option<String>> {
        let t = self.get_txn()?;
        let row: Option<SettingsPgRow> = query_as("SELECT tc_config FROM settings LIMIT 1")
            .fetch_optional(&mut **t)
            .await?;
        Ok(row
            .and_then(|r| r.tc_config)
            .map(|v| {
                serde_json::to_string(&v)
                    .map_err(|e| Error::Database(format!("get_tc_config serialize: {e}")))
            })
            .transpose()?)
    }

    async fn set_tc_config(&mut self, value: String) -> Result<()> {
        let json_val: serde_json::Value = serde_json::from_str(&value)
            .map_err(|e| Error::Database(format!("set_tc_config parse: {e}")))?;
        let t = self.get_txn()?;
        let n = query::<Postgres>("UPDATE settings SET tc_config = $1")
            .bind(Json(json_val))
            .execute(&mut **t)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::Database(
                "set_tc_config: no settings row found — \
                 the Supabase trigger must seed it on first user login"
                    .into(),
            ));
        }
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        let t = self
            .txn
            .take()
            .ok_or_else(|| Error::Database("Transaction already committed".into()))?;
        t.commit().await?;
        Ok(())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_to_datetime_utc_none() {
        assert_eq!(iso_to_datetime_utc(&None).unwrap(), None);
    }

    #[test]
    fn iso_to_datetime_utc_valid_rfc3339() {
        let ts = "2024-01-15T10:30:00Z".to_string();
        let result = iso_to_datetime_utc(&Some(ts)).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn iso_to_datetime_utc_valid_z_suffix() {
        let ts = "2024-01-15T10:30:00+00:00".to_string();
        let result = iso_to_datetime_utc(&Some(ts)).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn iso_to_datetime_utc_invalid() {
        let ts = "not-a-timestamp".to_string();
        assert!(iso_to_datetime_utc(&Some(ts)).is_err());
    }

    #[test]
    fn opt_str_to_uuid_none() {
        assert_eq!(opt_str_to_uuid(&None).unwrap(), None);
    }

    #[test]
    fn opt_str_to_uuid_valid() {
        let uuid_str = "550e8400-e29b-41d4-a716-446655440000".to_string();
        let result = opt_str_to_uuid(&Some(uuid_str)).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn opt_str_to_uuid_invalid() {
        let uuid_str = "not-a-uuid".to_string();
        assert!(opt_str_to_uuid(&Some(uuid_str)).is_err());
    }
}
