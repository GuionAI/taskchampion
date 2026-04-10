//! Postgres storage backend via pgwire.
//!
//! Connects to `pgwire-supabase-proxy` using a Supabase JWT for authentication.
//! Uses `tokio_postgres::Transaction` for real Postgres transactions.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sea_query::{
    Alias, Expr, ExprTrait, IntoColumnRef, JoinType, PostgresQueryBuilder, Query, SimpleExpr,
};
use sea_query_postgres::PostgresBinder;
use serde_json::Value as JsonValue;
use tokio_postgres::{Client, NoTls, Transaction};
use uuid::Uuid;

use self::iden::{Projects, Settings, TcTasks};
use crate::errors::{Error, Result};
use crate::operation::Operation;
use crate::storage::columns::raw_to_task;
use crate::storage::sql_ops::prepare_task;
use crate::storage::{Storage, StorageTxn, TaskMap};

mod iden;
mod row;
mod row_reader;
use row::{SettingsPgRow, TaskPgRow};
use row_reader::rows_to_tasks;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Convert an ISO8601 string from PreparedTask into a chrono DateTime<Utc>
/// for sea-query timestamptz binding. Returns None for None input.
/// Errors on a non-empty string that fails to parse — that's a data bug
/// upstream in `prepare_task`, surfaced loudly rather than silently dropped.
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

/// Convert an Option<String> UUID to Option<Uuid> for sea-query UUID binding.
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

/// Cast a `serde_json::Value` to jsonb so Postgres accepts it as a jsonb
/// column. Emits `CAST($N AS jsonb)`.
fn jsonb_write_json(value: serde_json::Value) -> SimpleExpr {
    Expr::value(value).cast_as(Alias::new("jsonb"))
}

/// Cast a jsonb column to text in a SELECT so tokio-postgres can decode
/// it into a Rust `String`. Emits `CAST("col" AS text)`.
///
/// Currently used only for `TcTasks::Data` and `Settings::TcConfig` (jsonb
/// columns that must be passed as strings to `serde_json::from_str`).
fn jsonb_read<C>(col: C) -> SimpleExpr
where
    C: IntoColumnRef,
{
    Expr::col(col).cast_as(Alias::new("text"))
}

/// Apply the standard task SELECT column projection to a sea-query
/// `SelectStatement`.
///
/// `data` stays cast-to-text via [`jsonb_read`] so `raw_to_task` can call
/// `serde_json::from_str` on it. All other columns are decoded natively by
/// tokio-postgres (`with-uuid-1` + `with-chrono-0_4` features) — no
/// SQL-layer casts required.
///
/// Mirrors the schema consumed by [`row_reader::read_raw_task_row`], which
/// uses string-based `try_get("name")` lookups — column ordering here is
/// not load-bearing, but column **aliases** are.
fn select_task_cols(q: &mut sea_query::SelectStatement, t: &Alias, p: &Alias) {
    q.column((t.clone(), TcTasks::Id))
        .expr_as(jsonb_read((t.clone(), TcTasks::Data)), Alias::new("data"))
        .column((t.clone(), TcTasks::Status))
        .column((t.clone(), TcTasks::Description))
        .column((t.clone(), TcTasks::Priority))
        .column((t.clone(), TcTasks::EntryAt))
        .column((t.clone(), TcTasks::ModifiedAt))
        .column((t.clone(), TcTasks::DueAt))
        .column((t.clone(), TcTasks::ScheduledAt))
        .column((t.clone(), TcTasks::StartAt))
        .column((t.clone(), TcTasks::EndAt))
        .column((t.clone(), TcTasks::WaitAt))
        .column((t.clone(), TcTasks::ParentId))
        .expr_as(
            Expr::col((p.clone(), Projects::Name)),
            Alias::new("project_name"),
        )
        .column((t.clone(), TcTasks::ProjectId))
        .column((t.clone(), TcTasks::NoteId));
}

// ── PgWireStorage ──────────────────────────────────────────────────────────

/// Postgres-backed storage that connects via the pgwire-supabase-proxy.
///
/// The proxy validates Supabase JWTs per-connection and sets RLS context.
/// Pass `DATABASE_URL` (full postgres:// URL with JWT as password) and `FLICKNOTE_TOKEN`
/// (Supabase JWT, for user_id extraction from JWT sub claim) to [`PgWireStorage::new`].
pub struct PgWireStorage {
    client: Client,
}

impl PgWireStorage {
    /// Connect to Postgres via pgwire.
    ///
    /// - `database_url`: Postgres connection string, e.g. `postgres://user:password@host:port/dbname`.
    ///   The JWT should be embedded as the password in the URL (the caller constructs this).
    /// - `token`: Supabase JWT. Kept for backwards compatibility — this parameter is no longer
    ///   used internally. The caller may use this token for user_id extraction (JWT sub claim).
    ///
    /// # Connection background task
    ///
    /// tokio-postgres requires a background task to drive the connection protocol.
    /// This task is spawned on the current tokio runtime. If it encounters a network
    /// error, subsequent operations on the returned `PgWireStorage` will fail with
    /// opaque "connection closed" errors from tokio-postgres.
    pub async fn new(database_url: &str, _token: &str) -> Result<Self> {
        /* pgwire connect */
        let (client, connection) = tokio_postgres::connect(database_url, NoTls).await?;

        // Spawn the connection task — it drives the protocol until the client drops.
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                log::error!("pgwire connection error: {e}");
            }
        });

        Ok(Self { client })
    }
}

#[async_trait]
impl Storage for PgWireStorage {
    async fn txn<'a>(&'a mut self) -> Result<Box<dyn StorageTxn + Send + 'a>> {
        /* begin transaction */
        let txn = self.client.transaction().await.map_err(|e| {
            Error::Database(format!(
                "begin transaction: {}",
                crate::errors::format_pg_err(&e)
            ))
        })?;
        Ok(Box::new(PgWireTxn { txn: Some(txn) }))
    }
}

// ── PgWireTxn ─────────────────────────────────────────────────────────────

pub(super) struct PgWireTxn<'a> {
    txn: Option<Transaction<'a>>,
}

impl<'a> PgWireTxn<'a> {
    fn get_txn(&self) -> Result<&Transaction<'a>> {
        self.txn
            .as_ref()
            .ok_or_else(|| Error::Database("Transaction already committed".into()))
    }

    /// Check if a task with the given UUID exists.
    async fn task_exists(&self, uuid: Uuid) -> Result<bool> {
        let t = self.get_txn()?;
        let (sql, vals) = Query::select()
            .expr(Expr::exists(
                Query::select()
                    // NOTE: must use `Expr::cust("1")` (inline SQL literal), NOT
                    // `Expr::val(1)`.  `Expr::val(1)` generates a parameterized `$1`
                    // which PostgreSQL infers as `text` inside EXISTS (no type context).
                    // `i32::to_sql(Type::TEXT, …)` then writes binary i32 bytes
                    // (`\x00\x00\x00\x01`) — null bytes cause
                    // "invalid byte sequence for encoding UTF8: 0x00".
                    .expr(Expr::cust("1"))
                    .from(TcTasks::Table)
                    .and_where(Expr::col(TcTasks::Id).eq(uuid))
                    .take(),
            ))
            .build_postgres(PostgresQueryBuilder);
        /* task_exists query */
        let row = t
            .query_one(sql.as_str(), &vals.as_params())
            .await
            .map_err(|e| {
                Error::Database(format!(
                    "task_exists query: {}",
                    crate::errors::format_pg_err(&e)
                ))
            })?;
        /* task_exists get */
        match row.try_get::<_, bool>(0) {
            Ok(v) => Ok(v),
            Err(e) => Err(Error::PgWire(e)),
        }
    }

    /// Resolve a project name to its UUID via the projects table.
    async fn resolve_project_id(&self, name: &str) -> Result<String> {
        let t = self.get_txn()?;
        let (sql, vals) = Query::select()
            .column(Projects::Id)
            .from(Projects::Table)
            .and_where(Expr::col(Projects::Name).eq(name))
            .order_by(Projects::CreatedAt, sea_query::Order::Asc)
            .limit(1)
            .build_postgres(PostgresQueryBuilder);
        /* resolve_project_id query */
        let row = t
            .query_opt(sql.as_str(), &vals.as_params())
            .await
            .map_err(|e| {
                Error::Database(format!(
                    "resolve_project_id query (name={name}): {}",
                    crate::errors::format_pg_err(&e)
                ))
            })?;
        match row {
            Some(row) => {
                /* resolve_project_id get id */
                let id: Uuid = match row.try_get(0) {
                    Ok(v) => v,
                    Err(e) => return Err(Error::PgWire(e)),
                };
                Ok(id.to_string())
            }
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
impl StorageTxn for PgWireTxn<'_> {
    async fn get_task(&mut self, uuid: Uuid) -> Result<Option<TaskMap>> {
        let t = self.get_txn()?;
        let t_alias = Alias::new("t");
        let p_alias = Alias::new("p");
        let mut q = Query::select();
        select_task_cols(&mut q, &t_alias, &p_alias);
        let (sql, vals) = q
            .from_as(TcTasks::Table, t_alias.clone())
            .join_as(
                JoinType::LeftJoin,
                Projects::Table,
                p_alias.clone(),
                Expr::col((t_alias.clone(), TcTasks::ProjectId))
                    .equals((p_alias.clone(), Projects::Id)),
            )
            .and_where(Expr::col((t_alias, TcTasks::Id)).eq(uuid))
            .limit(1)
            .build_postgres(PostgresQueryBuilder);
        /* get_task query */
        let rows = t
            .query(sql.as_str(), &vals.as_params())
            .await
            .map_err(|e| {
                Error::Database(format!(
                    "get_task query (uuid={uuid}): {}",
                    crate::errors::format_pg_err(&e)
                ))
            })?;
        match rows.into_iter().next() {
            None => Ok(None),
            Some(row) => {
                // Log UUID before deserializing `data` column — if it contains a 0x00
                // byte the error fires inside `from_row`, so this is the only chance
                // to emit the task ID in the trace.
                log::debug!("pgwire: get_task deserializing {uuid}");
                let raw = TaskPgRow::from_row(&row)?.into();
                let (_, task_map) = raw_to_task(raw)?;
                Ok(Some(task_map))
            }
        }
    }

    async fn get_pending_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let t = self.get_txn()?;
        let t_alias = Alias::new("t");
        let p_alias = Alias::new("p");
        let mut q = Query::select();
        select_task_cols(&mut q, &t_alias, &p_alias);
        let (sql, vals) = q
            .from_as(TcTasks::Table, t_alias.clone())
            .join_as(
                JoinType::LeftJoin,
                Projects::Table,
                p_alias.clone(),
                Expr::col((t_alias.clone(), TcTasks::ProjectId))
                    .equals((p_alias.clone(), Projects::Id)),
            )
            .and_where(Expr::col((t_alias, TcTasks::Status)).eq("pending"))
            .build_postgres(PostgresQueryBuilder);
        /* get_pending_tasks */
        let rows = t
            .query(sql.as_str(), &vals.as_params())
            .await
            .map_err(|e| {
                Error::Database(format!(
                    "get_pending_tasks query: {}",
                    crate::errors::format_pg_err(&e)
                ))
            })?;
        rows_to_tasks(rows)
    }

    async fn create_task(&mut self, uuid: Uuid) -> Result<bool> {
        if self.task_exists(uuid).await? {
            return Ok(false);
        }
        let t = self.get_txn()?;
        let (sql, vals) = Query::insert()
            .into_table(TcTasks::Table)
            .columns([TcTasks::Id, TcTasks::Data])
            .values_panic([
                uuid.into(),
                jsonb_write_json(JsonValue::Object(Default::default())),
            ])
            .build_postgres(PostgresQueryBuilder);
        /* create_task insert */
        t.execute(sql.as_str(), &vals.as_params())
            .await
            .map_err(|e| {
                Error::Database(format!(
                    "create_task insert (uuid={uuid}): {}",
                    crate::errors::format_pg_err(&e)
                ))
            })?;
        Ok(true)
    }

    async fn set_task(&mut self, uuid: Uuid, task: TaskMap) -> Result<()> {
        let prepared = prepare_task(task)?;

        let project_id_str = if let Some(name) = &prepared.project_name {
            Some(self.resolve_project_id(name).await?)
        } else {
            prepared.project_id_raw.clone()
        };
        let project_id = opt_str_to_uuid(&project_id_str)?;
        let parent_id = opt_str_to_uuid(&prepared.parent_id)?;

        let entry_at = iso_to_datetime_utc(&prepared.entry_at)?;
        let modified_at = iso_to_datetime_utc(&prepared.modified_at)?;
        let due_at = iso_to_datetime_utc(&prepared.due_at)?;
        let scheduled_at = iso_to_datetime_utc(&prepared.scheduled_at)?;
        let start_at = iso_to_datetime_utc(&prepared.start_at)?;
        let end_at = iso_to_datetime_utc(&prepared.end_at)?;
        let wait_at = iso_to_datetime_utc(&prepared.wait_at)?;
        let note_id = opt_str_to_uuid(&prepared.note_id)?;

        let data_val: JsonValue = serde_json::from_str(&prepared.data_json)
            .map_err(|e| Error::Database(format!("set_task parse data: {e}")))?;

        if self.task_exists(uuid).await? {
            let t = self.get_txn()?;
            let (sql, vals) = Query::update()
                .table(TcTasks::Table)
                .values([
                    (TcTasks::Data, jsonb_write_json(data_val.clone())),
                    (TcTasks::Status, prepared.status.clone().into()),
                    (TcTasks::Description, prepared.description.clone().into()),
                    (TcTasks::Priority, prepared.priority.clone().into()),
                    (TcTasks::EntryAt, entry_at.into()),
                    (TcTasks::ModifiedAt, modified_at.into()),
                    (TcTasks::DueAt, due_at.into()),
                    (TcTasks::ScheduledAt, scheduled_at.into()),
                    (TcTasks::StartAt, start_at.into()),
                    (TcTasks::EndAt, end_at.into()),
                    (TcTasks::WaitAt, wait_at.into()),
                    (TcTasks::ParentId, parent_id.into()),
                    (TcTasks::ProjectId, project_id.into()),
                    (TcTasks::NoteId, note_id.into()),
                ])
                .and_where(Expr::col(TcTasks::Id).eq(uuid))
                .build_postgres(PostgresQueryBuilder);
            /* set_task update */
            t.execute(sql.as_str(), &vals.as_params())
                .await
                .map_err(|e| {
                    Error::Database(format!(
                        "set_task update (uuid={uuid}): {}",
                        crate::errors::format_pg_err(&e)
                    ))
                })?;
        } else {
            let t = self.get_txn()?;
            let (sql, vals) = Query::insert()
                .into_table(TcTasks::Table)
                .columns([
                    TcTasks::Id,
                    TcTasks::Data,
                    TcTasks::Status,
                    TcTasks::Description,
                    TcTasks::Priority,
                    TcTasks::EntryAt,
                    TcTasks::ModifiedAt,
                    TcTasks::DueAt,
                    TcTasks::ScheduledAt,
                    TcTasks::StartAt,
                    TcTasks::EndAt,
                    TcTasks::WaitAt,
                    TcTasks::ParentId,
                    TcTasks::ProjectId,
                    TcTasks::NoteId,
                ])
                .values_panic([
                    uuid.into(),
                    jsonb_write_json(data_val),
                    prepared.status.into(),
                    prepared.description.into(),
                    prepared.priority.into(),
                    entry_at.into(),
                    modified_at.into(),
                    due_at.into(),
                    scheduled_at.into(),
                    start_at.into(),
                    end_at.into(),
                    wait_at.into(),
                    parent_id.into(),
                    project_id.into(),
                    note_id.into(),
                ])
                .build_postgres(PostgresQueryBuilder);
            /* set_task insert */
            t.execute(sql.as_str(), &vals.as_params())
                .await
                .map_err(|e| {
                    Error::Database(format!(
                        "set_task insert (uuid={uuid}): {}",
                        crate::errors::format_pg_err(&e)
                    ))
                })?;
        }
        Ok(())
    }

    async fn delete_task(&mut self, uuid: Uuid) -> Result<bool> {
        if self.task_exists(uuid).await? {
            let t = self.get_txn()?;
            let (sql, vals) = Query::delete()
                .from_table(TcTasks::Table)
                .and_where(Expr::col(TcTasks::Id).eq(uuid))
                .build_postgres(PostgresQueryBuilder);
            /* delete_task */
            let n = t
                .execute(sql.as_str(), &vals.as_params())
                .await
                .map_err(|e| {
                    Error::Database(format!(
                        "delete_task (uuid={uuid}): {}",
                        crate::errors::format_pg_err(&e)
                    ))
                })?;
            Ok(n > 0)
        } else {
            Ok(false)
        }
    }

    async fn all_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let t = self.get_txn()?;
        let t_alias = Alias::new("t");
        let p_alias = Alias::new("p");
        let mut q = Query::select();
        select_task_cols(&mut q, &t_alias, &p_alias);
        let (sql, vals) = q
            .from_as(TcTasks::Table, t_alias.clone())
            .join_as(
                JoinType::LeftJoin,
                Projects::Table,
                p_alias.clone(),
                Expr::col((t_alias.clone(), TcTasks::ProjectId))
                    .equals((p_alias.clone(), Projects::Id)),
            )
            .build_postgres(PostgresQueryBuilder);
        /* all_tasks */
        let rows = t
            .query(sql.as_str(), &vals.as_params())
            .await
            .map_err(|e| {
                Error::Database(format!(
                    "all_tasks query: {}",
                    crate::errors::format_pg_err(&e)
                ))
            })?;
        rows_to_tasks(rows)
    }

    async fn all_task_uuids(&mut self) -> Result<Vec<Uuid>> {
        let t = self.get_txn()?;
        let (sql, vals) = Query::select()
            .column(TcTasks::Id)
            .from(TcTasks::Table)
            .build_postgres(PostgresQueryBuilder);
        /* all_task_uuids */
        let rows = t
            .query(sql.as_str(), &vals.as_params())
            .await
            .map_err(|e| {
                Error::Database(format!(
                    "all_task_uuids query: {}",
                    crate::errors::format_pg_err(&e)
                ))
            })?;
        rows.into_iter()
            .map(|r| match r.try_get::<_, Uuid>(0) {
                Ok(v) => Ok(v),
                Err(e) => Err(Error::PgWire(e)),
            })
            .collect()
    }

    async fn get_task_operations(&mut self, _uuid: Uuid) -> Result<Vec<Operation>> {
        // pgwire backend has no operation log. Return empty — callers that need history
        // should use a local backend (powersync / external / inmemory). This is
        // consistent with add_operation being a silent no-op on write.
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
        // pgwire backend has no operation log — operations are local-only state
        // tracked by the replica. The remote Postgres is the source of truth for
        // task data; replaying operations against it is meaningless. We accept
        // the call silently so commit_operations() — the canonical write path that
        // calls this once per operation in the same write txn — continues to work
        // for set_task / create_task. Returning Err here would brick all writes.
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
        let sql = "SELECT DISTINCT kv.key AS name \
                   FROM tc_tasks, jsonb_each_text(data) AS kv \
                   WHERE kv.key LIKE 'tag_%' \
                   ORDER BY name";
        /* get_all_tags */
        let rows = t.query(sql, &[]).await.map_err(|e| {
            Error::Database(format!(
                "get_all_tags query: {}",
                crate::errors::format_pg_err(&e)
            ))
        })?;
        rows.into_iter()
            .map(|r| {
                /* read tag key */
                let key: String = match r.try_get(0) {
                    Ok(v) => v,
                    Err(e) => return Err(Error::PgWire(e)),
                };
                Ok(key.strip_prefix("tag_").unwrap_or(&key).to_string())
            })
            .collect()
    }

    async fn get_tc_config(&mut self) -> Result<Option<String>> {
        let t = self.get_txn()?;
        let (sql, vals) = Query::select()
            .expr(jsonb_read(Settings::TcConfig))
            .from(Settings::Table)
            .limit(1)
            .build_postgres(PostgresQueryBuilder);
        /* get_tc_config */
        let rows = t
            .query(sql.as_str(), &vals.as_params())
            .await
            .map_err(|e| {
                Error::Database(format!(
                    "get_tc_config query: {}",
                    crate::errors::format_pg_err(&e)
                ))
            })?;
        match rows.into_iter().next() {
            None => Ok(None),
            Some(row) => Ok(SettingsPgRow::from_row(&row)?.tc_config),
        }
    }

    async fn set_tc_config(&mut self, value: String) -> Result<()> {
        // Parse the JSON string to a serde_json::Value before sending to Postgres.
        // `jsonb_write` (CAST text AS jsonb) produces text-format JSON without the
        // required binary jsonb header byte (\x01), causing "unsupported jsonb version".
        let json_val: serde_json::Value = serde_json::from_str(&value)
            .map_err(|e| Error::Database(format!("set_tc_config parse: {e}")))?;
        let t = self.get_txn()?;
        let (sql, vals) = Query::update()
            .table(Settings::Table)
            .value(Settings::TcConfig, jsonb_write_json(json_val))
            .build_postgres(PostgresQueryBuilder);
        /* set_tc_config */
        let n = t
            .execute(sql.as_str(), &vals.as_params())
            .await
            .map_err(|e| {
                Error::Database(format!(
                    "set_tc_config update: {}",
                    crate::errors::format_pg_err(&e)
                ))
            })?;
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
        /* commit */
        t.commit().await.map_err(|e| {
            Error::Database(format!("commit: {}", crate::errors::format_pg_err(&e)))
        })?;
        Ok(())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod test {
    use sea_query_postgres::PostgresBinder;

    // Unit tests for helpers — no DB required.

    #[test]
    fn iso_to_datetime_utc_none() {
        assert_eq!(super::iso_to_datetime_utc(&None).unwrap(), None);
    }

    #[test]
    fn iso_to_datetime_utc_valid_rfc3339() {
        let s = Some("2024-08-25T19:06:11+00:00".to_string());
        let dt = super::iso_to_datetime_utc(&s).unwrap().unwrap();
        assert_eq!(dt.timestamp(), 1724612771);
    }

    #[test]
    fn iso_to_datetime_utc_valid_z_suffix() {
        let s = Some("2024-08-25T19:06:11Z".to_string());
        let dt = super::iso_to_datetime_utc(&s).unwrap().unwrap();
        assert_eq!(dt.timestamp(), 1724612771);
    }

    #[test]
    fn iso_to_datetime_utc_invalid() {
        let s = Some("not-an-iso".to_string());
        assert!(super::iso_to_datetime_utc(&s).is_err());
    }

    #[test]
    fn opt_str_to_uuid_none() {
        assert_eq!(super::opt_str_to_uuid(&None).unwrap(), None);
    }

    #[test]
    fn opt_str_to_uuid_valid() {
        let s = Some("550e8400-e29b-41d4-a716-446655440000".to_string());
        let uuid = super::opt_str_to_uuid(&s).unwrap().unwrap();
        assert_eq!(uuid.to_string(), "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn opt_str_to_uuid_invalid() {
        let s = Some("not-a-uuid".to_string());
        assert!(super::opt_str_to_uuid(&s).is_err());
    }

    #[test]
    fn get_tc_config_sql_casts_jsonb_to_text() {
        let (sql, _): (String, _) = sea_query::Query::select()
            .expr(super::jsonb_read(super::Settings::TcConfig))
            .from(super::Settings::Table)
            .limit(1)
            .build_postgres(sea_query::PostgresQueryBuilder);
        assert!(
            sql.contains("CAST(") && sql.to_lowercase().contains("as text"),
            "expected jsonb_read cast in SQL, got: {sql}"
        );
    }

    #[test]
    fn set_tc_config_sql_casts_value_to_jsonb() {
        let (sql, _): (String, _) = sea_query::Query::update()
            .table(super::Settings::Table)
            .value(
                super::Settings::TcConfig,
                super::jsonb_write_json(serde_json::json!({})),
            )
            .build_postgres(sea_query::PostgresQueryBuilder);
        assert!(
            sql.contains("CAST(") && sql.to_lowercase().contains("as jsonb"),
            "expected jsonb_write_json cast in SQL, got: {sql}"
        );
    }

    #[test]
    fn task_select_cols_casts_data_to_text() {
        let t = sea_query::Alias::new("t");
        let p = sea_query::Alias::new("p");
        let mut q = sea_query::Query::select();
        super::select_task_cols(&mut q, &t, &p);
        let (sql, _): (String, _) = q
            .from_as(super::TcTasks::Table, t.clone())
            .build_postgres(sea_query::PostgresQueryBuilder);
        // Only the `data` column (jsonb) should be cast to text — all uuid and
        // timestamptz columns are now decoded natively by tokio-postgres.
        assert!(
            sql.to_lowercase().contains("as text"),
            "expected data column cast to text, got: {sql}"
        );
        // Aliases must match what read_raw_task_row looks up.
        for alias in [
            "id",
            "data",
            "status",
            "description",
            "priority",
            "entry_at",
            "modified_at",
            "due_at",
            "scheduled_at",
            "start_at",
            "end_at",
            "wait_at",
            "parent_id",
            "project_name",
            "project_id",
        ] {
            assert!(
                sql.contains(alias),
                "expected alias {alias} in SQL, got: {sql}"
            );
        }
    }

    #[test]
    fn resolve_project_id_decodes_uuid_natively() {
        let (sql, _): (String, _) = sea_query::Query::select()
            .column(super::Projects::Id)
            .from(super::Projects::Table)
            .build_postgres(sea_query::PostgresQueryBuilder);
        // Projects::Id is a uuid column decoded natively — no SQL cast.
        assert!(
            !sql.to_lowercase().contains("cast"),
            "expected no CAST for Projects::Id (decoded natively), got: {sql}"
        );
    }

    #[test]
    fn all_task_uuids_decodes_uuid_natively() {
        let (sql, _): (String, _) = sea_query::Query::select()
            .column(super::TcTasks::Id)
            .from(super::TcTasks::Table)
            .build_postgres(sea_query::PostgresQueryBuilder);
        // TcTasks::Id is a uuid column decoded natively — no SQL cast.
        assert!(
            !sql.to_lowercase().contains("cast"),
            "expected no CAST for TcTasks::Id (decoded natively), got: {sql}"
        );
    }

    // Integration tests requiring a live Postgres. Run with:
    //   DATABASE_URL=postgres://user:JWT@host:port/dbname FLICKNOTE_TOKEN=... \
    //     cargo test --features storage-pgwire -- pgwire
    // Not run in CI (no Postgres available).

    async fn storage() -> Option<super::PgWireStorage> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let token = std::env::var("FLICKNOTE_TOKEN").ok()?;
        super::PgWireStorage::new(&url, &token).await.ok()
    }

    #[tokio::test]
    #[ignore]
    async fn drop_transaction() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::drop_transaction(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn create() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::create(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn create_exists() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::create_exists(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn get_missing() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::get_missing(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn set_task() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::set_task(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn delete_task_missing() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::delete_task_missing(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn delete_task_exists() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::delete_task_exists(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn all_tasks_empty() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::all_tasks_empty(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn all_tasks_and_uuids() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::all_tasks_and_uuids(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn get_all_tags() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::get_all_tags(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn tc_config_absent_returns_none() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::tc_config_absent_returns_none(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn tc_config_overwrite() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::tc_config_overwrite(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn tc_config_round_trip() -> crate::errors::Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::tc_config_round_trip(s).await
    }
}
