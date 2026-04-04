use tokio_postgres::Row;

use crate::errors::{Error, Result};
use crate::storage::columns::{raw_to_task, RawTaskRow};
use crate::storage::TaskMap;
use uuid::Uuid;

/// Read a `RawTaskRow` from a `tokio_postgres::Row`.
///
/// Expects the row to use `t.data::text` in the SELECT (the JSONB column cast
/// to TEXT), so `data` arrives as a plain string and can be read directly.
pub(super) fn read_raw_task_row(r: &Row) -> Result<RawTaskRow> {
    Ok(RawTaskRow {
        id: r
            .try_get("id")
            .map_err(|e| Error::Database(format!("read id: {e}")))?,
        data: r
            .try_get("data")
            .map_err(|e| Error::Database(format!("read data: {e}")))?,
        status: r
            .try_get("status")
            .map_err(|e| Error::Database(format!("read status: {e}")))?,
        description: r
            .try_get("description")
            .map_err(|e| Error::Database(format!("read description: {e}")))?,
        priority: r
            .try_get("priority")
            .map_err(|e| Error::Database(format!("read priority: {e}")))?,
        entry_at: r
            .try_get("entry_at")
            .map_err(|e| Error::Database(format!("read entry_at: {e}")))?,
        modified_at: r
            .try_get("modified_at")
            .map_err(|e| Error::Database(format!("read modified_at: {e}")))?,
        due_at: r
            .try_get("due_at")
            .map_err(|e| Error::Database(format!("read due_at: {e}")))?,
        scheduled_at: r
            .try_get("scheduled_at")
            .map_err(|e| Error::Database(format!("read scheduled_at: {e}")))?,
        start_at: r
            .try_get("start_at")
            .map_err(|e| Error::Database(format!("read start_at: {e}")))?,
        end_at: r
            .try_get("end_at")
            .map_err(|e| Error::Database(format!("read end_at: {e}")))?,
        wait_at: r
            .try_get("wait_at")
            .map_err(|e| Error::Database(format!("read wait_at: {e}")))?,
        parent_id: r
            .try_get("parent_id")
            .map_err(|e| Error::Database(format!("read parent_id: {e}")))?,
        project_name: r
            .try_get("project_name")
            .map_err(|e| Error::Database(format!("read project_name: {e}")))?,
        project_id: r
            .try_get("project_id")
            .map_err(|e| Error::Database(format!("read project_id: {e}")))?,
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
