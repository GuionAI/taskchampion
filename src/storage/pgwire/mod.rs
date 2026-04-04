//! Postgres storage backend via pgwire.
//!
//! Connects to `pgwire-supabase-proxy` using a Supabase JWT for authentication.
//! Uses `tokio_postgres::Transaction` for real Postgres transactions.

use async_trait::async_trait;
use tokio_postgres::{Client, NoTls, Transaction};
use uuid::Uuid;

use crate::errors::{Error, Result};
use crate::operation::Operation;
use crate::storage::columns::raw_to_task;
use crate::storage::sql_ops::prepare_task;
use crate::storage::{Storage, StorageTxn, TaskMap};

mod row_reader;
use row_reader::{read_raw_task_row, rows_to_tasks};

// ── SQL constants (Postgres-native $N placeholders) ────────────────────────

/// Column list used in all tc_tasks SELECT queries.
/// Mirrors the shared `TASK_SELECT_COLS` but in Postgres SQL syntax.
const PG_TASK_SELECT_COLS: &str = "t.id, t.data::text, t.status, t.description, t.priority, \
    t.entry_at, t.modified_at, t.due_at, t.scheduled_at, \
    t.start_at, t.end_at, t.wait_at, t.parent_id, \
    p.name as project_name, t.project_id";

const PG_TASK_EXISTS_SQL: &str =
    "SELECT EXISTS(SELECT 1 FROM tc_tasks WHERE id = $1) AS exists_flag";

const PG_ALL_OPERATIONS_SQL: &str = "SELECT data FROM tc_operations ORDER BY id ASC";
const PG_LAST_OPERATION_SQL: &str = "SELECT id, data FROM tc_operations ORDER BY id DESC LIMIT 1";
const PG_ALL_TASK_UUIDS_SQL: &str = "SELECT id FROM tc_tasks";
const PG_ALL_TAGS_SQL: &str = "SELECT DISTINCT kv.key AS name \
     FROM tc_tasks, jsonb_each_text(data) AS kv \
     WHERE kv.key LIKE 'tag_%' \
     ORDER BY name";
const PG_TC_CONFIG_READ_SQL: &str = "SELECT tc_config FROM settings LIMIT 1";

// ── PgWireStorage ──────────────────────────────────────────────────────────

/// Postgres-backed storage that connects via the pgwire-supabase-proxy.
///
/// The proxy validates Supabase JWTs per-connection and sets RLS context.
/// Pass `DATABASE_URL` (host/port/db, no password) and `FLICKNOTE_TOKEN`
/// (Supabase JWT) to [`PgWireStorage::new`].
pub struct PgWireStorage {
    client: Client,
}

impl PgWireStorage {
    /// Connect to Postgres via pgwire.
    ///
    /// - `database_url`: Postgres connection string, e.g. `postgres://host:port/dbname`
    ///   (no password — auth is via JWT in the user field)
    /// - `token`: Supabase JWT, sent as the pgwire `user` field for authentication
    pub async fn new(database_url: &str, token: &str) -> Result<Self> {
        // Parse the URL and inject the token as the user field.
        // tokio-postgres Config::from_str accepts standard DSN syntax.
        let config_str = build_config(database_url, token)?;

        let (client, connection) = tokio_postgres::connect(&config_str, NoTls)
            .await
            .map_err(|_| Error::Database("pgwire connect failed".into()))?;

        // Spawn the connection task — it drives the protocol until the client drops.
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                log::error!("pgwire connection error: {e}");
            }
        });

        Ok(Self { client })
    }
}

/// Build a tokio-postgres config string with the JWT as the user field.
fn build_config(database_url: &str, token: &str) -> Result<String> {
    // Parse the URL to extract host, port, and database.
    // Expected format: postgres://host:port/dbname or postgresql://host:port/dbname
    let without_scheme = database_url
        .strip_prefix("postgres://")
        .or_else(|| database_url.strip_prefix("postgresql://"))
        .ok_or_else(|| {
            Error::Database("DATABASE_URL must start with postgres:// or postgresql://".into())
        })?;

    // Split off path (database name).
    let (host_port, dbname) = match without_scheme.split_once('/') {
        Some((hp, db)) => (hp, db),
        None => (without_scheme, "postgres"),
    };

    // Split host and port.
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (h, p),
        None => (host_port, "5432"),
    };

    // The JWT may contain special characters; use key=value format.
    // We escape single quotes in the token (shouldn't happen in a JWT, but be safe).
    let escaped_token = token.replace('\'', "\\'");
    Ok(format!(
        "host={host} port={port} dbname={dbname} user='{escaped_token}'"
    ))
}

#[async_trait]
impl Storage for PgWireStorage {
    async fn txn<'a>(&'a mut self) -> Result<Box<dyn StorageTxn + Send + 'a>> {
        let txn = self
            .client
            .transaction()
            .await
            .map_err(|e| Error::Database(format!("begin transaction: {e}")))?;
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
}

/// Parse an operation from a JSON string, handling Supabase JSONB double-encoding.
///
/// Supabase JSONB can double-encode bare JSON string values:
/// `Operation::UndoPoint` serializes as `"UndoPoint"` (a JSON string), which
/// Supabase JSONB stores as a string value. When read back via pgwire text mode,
/// it may arrive as `"\"UndoPoint\""`. Object variants like `{"Create":{...}}`
/// are unaffected because JSON objects don't get re-wrapped.
pub(crate) fn parse_operation(data_str: &str) -> Result<Operation> {
    match serde_json::from_str::<Operation>(data_str) {
        Ok(op) => Ok(op),
        Err(original_err) => {
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

/// Decode an operation data value from tokio-postgres.
///
/// The `data` column is `jsonb` in Postgres. tokio-postgres decodes JSONB
/// columns as `serde_json::Value` (via the `with-serde_json-1` feature).
/// We re-serialize to a string so `parse_operation` can handle double-encoding.
fn decode_op_data(row: &tokio_postgres::Row, col: &str) -> Result<Operation> {
    let val: serde_json::Value = row
        .try_get(col)
        .map_err(|e| Error::Database(format!("read {col}: {e}")))?;
    let s = serde_json::to_string(&val)
        .map_err(|e| Error::Database(format!("serialize {col}: {e}")))?;
    parse_operation(&s)
}

#[async_trait]
impl StorageTxn for PgWireTxn<'_> {
    async fn get_task(&mut self, uuid: Uuid) -> Result<Option<TaskMap>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {PG_TASK_SELECT_COLS}
             FROM tc_tasks t
             LEFT JOIN projects p ON t.project_id = p.id
             WHERE t.id = $1 LIMIT 1"
        );
        let rows = t
            .query(&sql, &[&uuid.to_string()])
            .await
            .map_err(|e| Error::Database(format!("get_task query: {e}")))?;
        match rows.into_iter().next() {
            None => Ok(None),
            Some(row) => {
                let raw = read_raw_task_row(&row)?;
                let (_, task_map) = raw_to_task(raw)?;
                Ok(Some(task_map))
            }
        }
    }

    async fn get_pending_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {PG_TASK_SELECT_COLS}
             FROM tc_tasks t
             LEFT JOIN projects p ON t.project_id = p.id
             WHERE t.status = 'pending'"
        );
        let rows = t
            .query(&sql, &[])
            .await
            .map_err(|e| Error::Database(format!("get_pending_tasks: {e}")))?;
        rows_to_tasks(rows)
    }

    async fn create_task(&mut self, uuid: Uuid) -> Result<bool> {
        let t = self.get_txn()?;
        let exists: bool = t
            .query_one(PG_TASK_EXISTS_SQL, &[&uuid.to_string()])
            .await
            .map_err(|e| Error::Database(format!("create_task exists check: {e}")))?
            .try_get(0)
            .map_err(|e| Error::Database(format!("create_task exists get: {e}")))?;
        if exists {
            return Ok(false);
        }
        t.execute(
            "INSERT INTO tc_tasks (id, data) VALUES ($1, '{}'::jsonb)",
            &[&uuid.to_string()],
        )
        .await
        .map_err(|e| Error::Database(format!("create_task insert: {e}")))?;
        Ok(true)
    }

    async fn set_task(&mut self, uuid: Uuid, task: TaskMap) -> Result<()> {
        let prepared = prepare_task(task)?;
        let uuid_str = uuid.to_string();

        let t = self.get_txn()?;
        let exists: bool = t
            .query_one(PG_TASK_EXISTS_SQL, &[&uuid_str])
            .await
            .map_err(|e| Error::Database(format!("set_task exists check: {e}")))?
            .try_get(0)
            .map_err(|e| Error::Database(format!("set_task exists get: {e}")))?;

        // Parse data_json to serde_json::Value for JSONB binding.
        let data_val: serde_json::Value = serde_json::from_str(&prepared.data_json)
            .map_err(|e| Error::Database(format!("set_task parse data: {e}")))?;

        let project_id = prepared.project_id_raw.as_deref();

        if exists {
            t.execute(
                "UPDATE tc_tasks SET \
                 data = $1::jsonb, status = $2, description = $3, priority = $4, \
                 entry_at = $5, modified_at = $6, due_at = $7, scheduled_at = $8, \
                 start_at = $9, end_at = $10, wait_at = $11, parent_id = $12, project_id = $13 \
                 WHERE id = $14",
                &[
                    &data_val,
                    &prepared.status,
                    &prepared.description,
                    &prepared.priority,
                    &prepared.entry_at,
                    &prepared.modified_at,
                    &prepared.due_at,
                    &prepared.scheduled_at,
                    &prepared.start_at,
                    &prepared.end_at,
                    &prepared.wait_at,
                    &prepared.parent_id,
                    &project_id,
                    &uuid_str,
                ],
            )
            .await
            .map_err(|e| Error::Database(format!("set_task update: {e}")))?;
        } else {
            t.execute(
                "INSERT INTO tc_tasks \
                 (id, data, status, description, priority, \
                  entry_at, modified_at, due_at, scheduled_at, start_at, end_at, wait_at, \
                  parent_id, project_id) \
                 VALUES ($1, $2::jsonb, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
                &[
                    &uuid_str,
                    &data_val,
                    &prepared.status,
                    &prepared.description,
                    &prepared.priority,
                    &prepared.entry_at,
                    &prepared.modified_at,
                    &prepared.due_at,
                    &prepared.scheduled_at,
                    &prepared.start_at,
                    &prepared.end_at,
                    &prepared.wait_at,
                    &prepared.parent_id,
                    &project_id,
                ],
            )
            .await
            .map_err(|e| Error::Database(format!("set_task insert: {e}")))?;
        }
        Ok(())
    }

    async fn delete_task(&mut self, uuid: Uuid) -> Result<bool> {
        let t = self.get_txn()?;
        let uuid_str = uuid.to_string();
        let exists: bool = t
            .query_one(PG_TASK_EXISTS_SQL, &[&uuid_str])
            .await
            .map_err(|e| Error::Database(format!("delete_task exists check: {e}")))?
            .try_get(0)
            .map_err(|e| Error::Database(format!("delete_task exists get: {e}")))?;
        if exists {
            t.execute("DELETE FROM tc_tasks WHERE id = $1", &[&uuid_str])
                .await
                .map_err(|e| Error::Database(format!("delete_task: {e}")))?;
        }
        Ok(exists)
    }

    async fn all_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let t = self.get_txn()?;
        let sql = format!(
            "SELECT {PG_TASK_SELECT_COLS}
             FROM tc_tasks t
             LEFT JOIN projects p ON t.project_id = p.id"
        );
        let rows = t
            .query(&sql, &[])
            .await
            .map_err(|e| Error::Database(format!("all_tasks: {e}")))?;
        rows_to_tasks(rows)
    }

    async fn all_task_uuids(&mut self) -> Result<Vec<Uuid>> {
        let t = self.get_txn()?;
        let rows = t
            .query(PG_ALL_TASK_UUIDS_SQL, &[])
            .await
            .map_err(|e| Error::Database(format!("all_task_uuids: {e}")))?;
        rows.into_iter()
            .map(|r| {
                let s: String = r
                    .try_get(0)
                    .map_err(|e| Error::Database(format!("read uuid: {e}")))?;
                Uuid::parse_str(&s).map_err(|e| Error::Database(format!("invalid UUID: {e}")))
            })
            .collect()
    }

    async fn get_task_operations(&mut self, uuid: Uuid) -> Result<Vec<Operation>> {
        // Postgres schema has no UUID column on tc_operations; filter in-memory.
        let ops = self.all_operations().await?;
        Ok(ops
            .into_iter()
            .filter(|op| op.get_uuid() == Some(uuid))
            .collect())
    }

    async fn all_operations(&mut self) -> Result<Vec<Operation>> {
        let t = self.get_txn()?;
        let rows = t
            .query(PG_ALL_OPERATIONS_SQL, &[])
            .await
            .map_err(|e| Error::Database(format!("all_operations: {e}")))?;
        rows.iter().map(|r| decode_op_data(r, "data")).collect()
    }

    async fn add_operation(&mut self, op: Operation) -> Result<()> {
        use chrono::Utc;

        let created_at = match &op {
            Operation::Update { timestamp, .. } => {
                timestamp.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
            }
            _ => Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        };
        let data_str = serde_json::to_string(&op)
            .map_err(|e| Error::Database(format!("serialize operation: {e}")))?;
        let data_val: serde_json::Value = serde_json::from_str(&data_str)
            .map_err(|e| Error::Database(format!("parse operation json: {e}")))?;
        let id = Uuid::now_v7().to_string();

        let t = self.get_txn()?;
        t.execute(
            "INSERT INTO tc_operations (id, data, created_at) VALUES ($1, $2::jsonb, $3)",
            &[&id, &data_val, &created_at],
        )
        .await
        .map_err(|e| Error::Database(format!("add_operation: {e}")))?;
        Ok(())
    }

    async fn remove_operation(&mut self, op: Operation) -> Result<()> {
        let t = self.get_txn()?;
        let rows = t
            .query(PG_LAST_OPERATION_SQL, &[])
            .await
            .map_err(|e| Error::Database(format!("remove_operation query: {e}")))?;

        let last_row = rows
            .into_iter()
            .next()
            .ok_or_else(|| Error::Database("No operations to remove".into()))?;

        let last_id: String = last_row
            .try_get(0)
            .map_err(|e| Error::Database(format!("remove_operation id: {e}")))?;
        let last_op = decode_op_data(&last_row, "data")?;

        if last_op != op {
            return Err(Error::Database(format!(
                "Last operation does not match -- cannot remove \
                 (expected {op:?}, got {last_op:?})"
            )));
        }

        t.execute("DELETE FROM tc_operations WHERE id = $1", &[&last_id])
            .await
            .map_err(|e| Error::Database(format!("remove_operation delete: {e}")))?;
        Ok(())
    }

    async fn get_all_tags(&mut self) -> Result<Vec<String>> {
        let t = self.get_txn()?;
        let rows = t
            .query(PG_ALL_TAGS_SQL, &[])
            .await
            .map_err(|e| Error::Database(format!("get_all_tags: {e}")))?;
        rows.into_iter()
            .map(|r| {
                // Strip "tag_" prefix to return just the tag name.
                let key: String = r
                    .try_get(0)
                    .map_err(|e| Error::Database(format!("read tag key: {e}")))?;
                Ok(key.strip_prefix("tag_").unwrap_or(&key).to_string())
            })
            .collect()
    }

    async fn get_tc_config(&mut self) -> Result<Option<String>> {
        let t = self.get_txn()?;
        let rows = t
            .query(PG_TC_CONFIG_READ_SQL, &[])
            .await
            .map_err(|e| Error::Database(format!("get_tc_config: {e}")))?;
        match rows.into_iter().next() {
            None => Ok(None),
            Some(row) => {
                let val: Option<String> = row
                    .try_get(0)
                    .map_err(|e| Error::Database(format!("get_tc_config read: {e}")))?;
                Ok(val)
            }
        }
    }

    async fn set_tc_config(&mut self, value: String) -> Result<()> {
        let t = self.get_txn()?;
        t.execute("UPDATE settings SET tc_config = $1", &[&value])
            .await
            .map_err(|e| Error::Database(format!("set_tc_config: {e}")))?;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        let t = self
            .txn
            .take()
            .ok_or_else(|| Error::Database("Transaction already committed".into()))?;
        t.commit()
            .await
            .map_err(|e| Error::Database(format!("commit: {e}")))?;
        Ok(())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod test {
    use crate::errors::Result;

    // Tests for parse_operation — run without Postgres, always enabled.
    mod parse_tests {
        use super::super::parse_operation;

        #[test]
        fn normal_undo_point() {
            let data = r#""UndoPoint""#;
            let op = parse_operation(data).unwrap();
            assert!(op.is_undo_point());
        }

        #[test]
        fn double_encoded_undo_point() {
            let data = r#""\"UndoPoint\"""#;
            let op = parse_operation(data).unwrap();
            assert!(op.is_undo_point());
        }

        #[test]
        fn double_encoded_invalid_variant() {
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

    // Integration tests requiring a live Postgres. Run with:
    //   DATABASE_URL=postgres://host:port/dbname FLICKNOTE_TOKEN=... \
    //     cargo test --features storage-pgwire -- pgwire
    // Not run in CI (no Postgres available).

    async fn storage() -> Option<super::PgWireStorage> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let token = std::env::var("FLICKNOTE_TOKEN").ok()?;
        super::PgWireStorage::new(&url, &token).await.ok()
    }

    #[tokio::test]
    #[ignore]
    async fn drop_transaction() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::drop_transaction(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn create() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::create(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn create_exists() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::create_exists(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn get_missing() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::get_missing(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn set_task() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::set_task(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn delete_task_missing() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::delete_task_missing(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn delete_task_exists() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::delete_task_exists(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn all_tasks_empty() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::all_tasks_empty(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn all_tasks_and_uuids() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::all_tasks_and_uuids(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn task_operations() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::task_operations(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn get_all_tags() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::get_all_tags(s).await
    }

    #[tokio::test]
    #[ignore]
    async fn tc_config_absent_returns_none() -> Result<()> {
        let s = storage().await.unwrap();
        crate::storage::test::tc_config_absent_returns_none(s).await
    }
}
