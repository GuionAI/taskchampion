use chrono::{DateTime, Utc};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::errors::{Error, Result};
use crate::storage::columns::{raw_to_task, RawTaskRow};
use crate::storage::TaskMap;

/// Read a `RawTaskRow` from a `tokio_postgres::Row`.
///
/// Every non-text column is decoded into its native Rust wire type
/// (`Uuid`, `DateTime<Utc>`, `String` for jsonb-cast-to-text) and then
/// stringified into the shared all-`String` `RawTaskRow` struct.
///
/// Timestamps are formatted via `DateTime::<Utc>::to_rfc3339()`, which
/// produces strict RFC3339 (`2024-08-25T19:06:11+00:00`). This is the
/// exact format the downstream `iso_to_epoch` parser in `raw_to_task`
/// accepts — eliminating the format mismatch that pg's native
/// `timestamptz::text` cast introduces (`2024-08-25 19:06:11+00`).
///
/// The `data` column stays cast-to-text via `jsonb_read` in the SQL
/// projection because we still pass it to `serde_json::from_str` in
/// `raw_to_task`.
pub(super) fn read_raw_task_row(r: &Row) -> Result<RawTaskRow> {
    let id: Uuid = r
        .try_get("id")
        .map_err(|e| Error::Database(format!("read id: {e}")))?;
    let data: String = r
        .try_get("data")
        .map_err(|e| Error::Database(format!("read data: {e}")))?;
    let status: Option<String> = r
        .try_get("status")
        .map_err(|e| Error::Database(format!("read status: {e}")))?;
    let description: Option<String> = r
        .try_get("description")
        .map_err(|e| Error::Database(format!("read description: {e}")))?;
    let priority: Option<String> = r
        .try_get("priority")
        .map_err(|e| Error::Database(format!("read priority: {e}")))?;
    let entry_at: Option<DateTime<Utc>> = r
        .try_get("entry_at")
        .map_err(|e| Error::Database(format!("read entry_at: {e}")))?;
    let modified_at: Option<DateTime<Utc>> = r
        .try_get("modified_at")
        .map_err(|e| Error::Database(format!("read modified_at: {e}")))?;
    let due_at: Option<DateTime<Utc>> = r
        .try_get("due_at")
        .map_err(|e| Error::Database(format!("read due_at: {e}")))?;
    let scheduled_at: Option<DateTime<Utc>> = r
        .try_get("scheduled_at")
        .map_err(|e| Error::Database(format!("read scheduled_at: {e}")))?;
    let start_at: Option<DateTime<Utc>> = r
        .try_get("start_at")
        .map_err(|e| Error::Database(format!("read start_at: {e}")))?;
    let end_at: Option<DateTime<Utc>> = r
        .try_get("end_at")
        .map_err(|e| Error::Database(format!("read end_at: {e}")))?;
    let wait_at: Option<DateTime<Utc>> = r
        .try_get("wait_at")
        .map_err(|e| Error::Database(format!("read wait_at: {e}")))?;
    let parent_id: Option<Uuid> = r
        .try_get("parent_id")
        .map_err(|e| Error::Database(format!("read parent_id: {e}")))?;
    let project_name: Option<String> = r
        .try_get("project_name")
        .map_err(|e| Error::Database(format!("read project_name: {e}")))?;
    let project_id: Option<Uuid> = r
        .try_get("project_id")
        .map_err(|e| Error::Database(format!("read project_id: {e}")))?;

    Ok(RawTaskRow {
        id: id.to_string(),
        data,
        status,
        description,
        priority,
        entry_at: entry_at.map(|dt| dt.to_rfc3339()),
        modified_at: modified_at.map(|dt| dt.to_rfc3339()),
        due_at: due_at.map(|dt| dt.to_rfc3339()),
        scheduled_at: scheduled_at.map(|dt| dt.to_rfc3339()),
        start_at: start_at.map(|dt| dt.to_rfc3339()),
        end_at: end_at.map(|dt| dt.to_rfc3339()),
        wait_at: wait_at.map(|dt| dt.to_rfc3339()),
        parent_id: parent_id.map(|u| u.to_string()),
        project_name,
        project_id: project_id.map(|u| u.to_string()),
    })
}

/// Convert a list of rows to `(Uuid, TaskMap)` pairs.
pub(super) fn rows_to_tasks(rows: Vec<Row>) -> Result<Vec<(Uuid, TaskMap)>> {
    rows.into_iter()
        .map(|r| {
            let raw = read_raw_task_row(&r)?;
            raw_to_task(raw)
        })
        .collect()
}
