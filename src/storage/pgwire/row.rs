//! Postgres wire-native row types.
//!
//! These structs mirror actual Postgres column types with full type-checked
//! visibility into every column. Each `from_row` method deserializes from
//! `tokio_postgres::Row` using explicit `try_get::<_, T>` turbofish hints
//! (required because the compiler cannot infer `T` backwards through the
//! method chain reliably without them).
//!
//! The `From` impls transform native PG types into the shared `RawTaskRow`
//! string-based intermediate, which feeds into `raw_to_task` — the same
//! downstream domain conversion used by all storage backends.

use chrono::{DateTime, Utc};
use tokio_postgres::Row;
use uuid::Uuid;

use std::result::Result as StdResult;

use crate::errors::Error;
use crate::storage::columns::RawTaskRow;

// ─── Task ───────────────────────────────────────────────────────────────────

/// Intermediate Pg-side struct for a tc_tasks row.
///
/// Holds native Postgres wire types — `Uuid`, `DateTime<Utc>`, `String`.
///
/// Note: `parent_id` is stored as `uuid` in Postgres but can be NULL
/// even though the domain treats it as a foreign key.
pub(super) struct TaskPgRow {
    pub id: Uuid,
    pub data: String,
    pub status: Option<String>,
    pub description: Option<String>,
    pub priority: Option<String>,
    pub entry_at: Option<DateTime<Utc>>,
    pub modified_at: Option<DateTime<Utc>>,
    pub due_at: Option<DateTime<Utc>>,
    pub scheduled_at: Option<DateTime<Utc>>,
    pub start_at: Option<DateTime<Utc>>,
    pub end_at: Option<DateTime<Utc>>,
    pub wait_at: Option<DateTime<Utc>>,
    pub parent_id: Option<Uuid>,
    pub project_name: Option<String>,
    pub project_id: Option<Uuid>,
}

impl TaskPgRow {
    /// Deserialize from a `tokio_postgres::Row` into `TaskPgRow`.
    ///
    /// Uses explicit `try_get::<_, T>` turbofish on every non-String column.
    /// The `data` column is cast to text via `jsonb_read` in the SQL, so it
    /// deserializes as `String` — which `serde_json::from_str` in
    /// `raw_to_task` accepts directly.
    pub(super) fn from_row(row: &Row) -> StdResult<Self, Error> {
        Ok(Self {
            id: row.try_get::<_, Uuid>("id").map_err(Error::PgWire)?,
            data: row.try_get::<_, String>("data").map_err(Error::PgWire)?,
            status: row
                .try_get::<_, Option<String>>("status")
                .map_err(Error::PgWire)?,
            description: row
                .try_get::<_, Option<String>>("description")
                .map_err(Error::PgWire)?,
            priority: row
                .try_get::<_, Option<String>>("priority")
                .map_err(Error::PgWire)?,
            entry_at: row
                .try_get::<_, Option<DateTime<Utc>>>("entry_at")
                .map_err(Error::PgWire)?,
            modified_at: row
                .try_get::<_, Option<DateTime<Utc>>>("modified_at")
                .map_err(Error::PgWire)?,
            due_at: row
                .try_get::<_, Option<DateTime<Utc>>>("due_at")
                .map_err(Error::PgWire)?,
            scheduled_at: row
                .try_get::<_, Option<DateTime<Utc>>>("scheduled_at")
                .map_err(Error::PgWire)?,
            start_at: row
                .try_get::<_, Option<DateTime<Utc>>>("start_at")
                .map_err(Error::PgWire)?,
            end_at: row
                .try_get::<_, Option<DateTime<Utc>>>("end_at")
                .map_err(Error::PgWire)?,
            wait_at: row
                .try_get::<_, Option<DateTime<Utc>>>("wait_at")
                .map_err(Error::PgWire)?,
            parent_id: row
                .try_get::<_, Option<Uuid>>("parent_id")
                .map_err(Error::PgWire)?,
            project_name: row
                .try_get::<_, Option<String>>("project_name")
                .map_err(Error::PgWire)?,
            project_id: row
                .try_get::<_, Option<Uuid>>("project_id")
                .map_err(Error::PgWire)?,
        })
    }
}

impl From<TaskPgRow> for RawTaskRow {
    /// Convert native PG types into the shared all-String `RawTaskRow` intermediate.
    ///
    /// Timestamps use `to_rfc3339()` — this is the exact ISO 8601 format
    /// that `raw_to_task`'s downstream `iso_to_epoch` parser accepts, avoiding
    /// the format mismatch that Postgres's native `timestamptz::text` cast
    /// introduces (space-separated date+time, non-normalized offset).
    fn from(r: TaskPgRow) -> Self {
        Self {
            id: r.id.to_string(),
            data: r.data,
            status: r.status,
            description: r.description,
            priority: r.priority,
            entry_at: r.entry_at.map(|dt| dt.to_rfc3339()),
            modified_at: r.modified_at.map(|dt| dt.to_rfc3339()),
            due_at: r.due_at.map(|dt| dt.to_rfc3339()),
            scheduled_at: r.scheduled_at.map(|dt| dt.to_rfc3339()),
            start_at: r.start_at.map(|dt| dt.to_rfc3339()),
            end_at: r.end_at.map(|dt| dt.to_rfc3339()),
            wait_at: r.wait_at.map(|dt| dt.to_rfc3339()),
            parent_id: r.parent_id.map(|u| u.to_string()),
            project_name: r.project_name,
            project_id: r.project_id.map(|u| u.to_string()),
        }
    }
}

// ─── Settings ───────────────────────────────────────────────────────────────

/// Pg-side struct for the tc_settings singleton row.
///
/// The settings table has a single row identified by `id` TEXT PRIMARY KEY.
/// Only `tc_config` (JSONB) is queried — `id` is implicit.
pub(super) struct SettingsPgRow {
    pub tc_config: Option<String>,
}

impl SettingsPgRow {
    /// Deserialize from a `tokio_postgres::Row` into `SettingsPgRow`.
    ///
    /// The `tc_config` column is cast to text via `jsonb_read` in the SQL,
    /// so it deserializes as `String`.
    pub(super) fn from_row(row: &Row) -> StdResult<Self, Error> {
        Ok(Self {
            tc_config: row
                .try_get::<_, Option<String>>("tc_config")
                .map_err(Error::PgWire)?,
        })
    }
}
