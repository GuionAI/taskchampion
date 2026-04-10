//! Read rows from Postgres into domain types.
//!
//! All `try_get` deserialization is centralized in `row::TaskPgRow::from_row`.
//! This module exposes `rows_to_tasks` which applies the shared
//! `raw_to_task` domain conversion to every row.

use tokio_postgres::Row;
use uuid::Uuid;

use crate::errors::{Error, Result};
use crate::storage::columns::raw_to_task;
use crate::storage::TaskMap;

use super::row::TaskPgRow;

// ─── Read helpers ────────────────────────────────────────────────────────────

/// Convert a list of rows to `(Uuid, TaskMap)` pairs.
///
/// Delegates all PG deserialization to `TaskPgRow::from_row`, then uses
/// the shared `raw_to_task` domain conversion on each `RawTaskRow`.
pub(super) fn rows_to_tasks(rows: Vec<Row>) -> Result<Vec<(Uuid, TaskMap)>> {
    rows.into_iter()
        .map(|r| {
            // Extract UUID first (separate from full row deserialization) so we can
            // log it even if the `data` column is corrupted (e.g. 0x00 byte). UUID is
            // a native PG type stored independently of `data`, so this read is safe.
            let id: Uuid = r
                .try_get::<_, Uuid>("id")
                .map_err(Error::PgWire)?;
            log::debug!("pgwire: deserializing task {id}");
            let raw = TaskPgRow::from_row(&r)?.into();
            raw_to_task(raw)
        })
        .collect()
}
