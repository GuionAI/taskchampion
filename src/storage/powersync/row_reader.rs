use anyhow::Context;
use uuid::Uuid;

use crate::errors::Result;
use crate::storage::columns::{raw_to_task, RawTaskRow};
use crate::storage::TaskMap;

/// Read a `RawTaskRow` from a rusqlite Row (column-name based).
pub(super) fn read_raw_task_row(r: &rusqlite::Row) -> rusqlite::Result<RawTaskRow> {
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
        position: r.get("position")?,
        project_name: r.get("project_name")?,
    })
}

/// Execute a task SELECT query and convert each row to `(Uuid, TaskMap)`.
pub(super) fn query_task_rows(
    t: &rusqlite::Transaction<'_>,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<(Uuid, TaskMap)>> {
    let mut q = t
        .prepare(sql)
        .with_context(|| format!("Preparing query: {sql}"))?;
    let rows: Vec<RawTaskRow> = q
        .query_map(params, read_raw_task_row)?
        .collect::<std::result::Result<_, _>>()?;
    rows.into_iter().map(raw_to_task).collect()
}
