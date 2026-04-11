//! Read decoded rows into domain types.
//!
//! All PG deserialization is handled by the `FromRow` impl in `row.rs`.
//! This module exposes `rows_to_tasks` which applies the shared `raw_to_task`
//! domain conversion to each already-decoded `TaskPgRow`.

use uuid::Uuid;

use crate::errors::Result;
use crate::storage::columns::raw_to_task;
use crate::storage::TaskMap;

use super::row::TaskPgRow;

/// Convert a list of decoded rows to `(Uuid, TaskMap)` pairs.
///
/// Delegates all PG deserialization to `FromRow` in `TaskPgRow`, then uses
/// the shared `raw_to_task` domain conversion on each `RawTaskRow`.
pub(super) fn rows_to_tasks(rows: Vec<TaskPgRow>) -> Result<Vec<(Uuid, TaskMap)>> {
    rows.into_iter()
        .map(|r| {
            let id = r.id;
            log::debug!("pgwire: deserializing task {id}");
            let raw = r.into();
            raw_to_task(raw)
        })
        .collect()
}
