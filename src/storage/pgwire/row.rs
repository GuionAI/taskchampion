//! Postgres wire-native row types.
//!
//! sqlx decodes jsonb columns natively into `serde_json::Value` via the
//! `json` feature. `query_as!` constructs these structs at compile time, and
//! the `From<TaskPgRow>` impl re-serializes `Value` to feed the crate-level
//! `RawTaskRow` shared with powersync.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::storage::columns::RawTaskRow;

// ─── Task ───────────────────────────────────────────────────────────────────

/// Intermediate Pg-side struct for a tc_tasks row.
pub(super) struct TaskPgRow {
    pub(super) id: Uuid,
    pub(super) short_id: Option<i32>,
    pub(super) data: serde_json::Value,
    pub(super) status: Option<String>,
    pub(super) description: Option<String>,
    pub(super) priority: Option<String>,
    pub(super) entry_at: Option<DateTime<Utc>>,
    pub(super) modified_at: Option<DateTime<Utc>>,
    pub(super) due_at: Option<DateTime<Utc>>,
    pub(super) scheduled_at: Option<DateTime<Utc>>,
    pub(super) start_at: Option<DateTime<Utc>>,
    pub(super) end_at: Option<DateTime<Utc>>,
    pub(super) wait_at: Option<DateTime<Utc>>,
    pub(super) parent_id: Option<Uuid>,
    pub(super) project_name: Option<String>,
    pub(super) project_id: Option<Uuid>,
    pub(super) note_id: Option<Uuid>,
}

impl From<TaskPgRow> for RawTaskRow {
    /// Convert native PG types into the shared all-String `RawTaskRow` intermediate.
    ///
    /// `data: Value → String` re-serialization is the deliberate containment seam.
    /// The crate-level `RawTaskRow` stores `data: String` (shared with powersync).
    /// This module is the only place that pays the `Value → String` cost.
    fn from(r: TaskPgRow) -> Self {
        Self {
            id: r.id.to_string(),
            short_id: r.short_id.map(i64::from),
            data: serde_json::to_string(&r.data).expect("jsonb Value re-serialize cannot fail"),
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
            note_id: r.note_id.map(|u| u.to_string()),
        }
    }
}

// ─── Settings ───────────────────────────────────────────────────────────────

/// Pg-side struct for the tc_settings singleton row.
pub(super) struct SettingsPgRow {
    pub(super) tc_config: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn task_pg_row_to_raw_task_row_jsonb_roundtrip() {
        let row = TaskPgRow {
            id: Uuid::nil(),
            short_id: Some(42),
            data: json!({"description": "test", "status": "pending"}),
            status: Some("pending".into()),
            description: Some("test".into()),
            priority: None,
            entry_at: None,
            modified_at: None,
            due_at: None,
            scheduled_at: None,
            start_at: None,
            end_at: None,
            wait_at: None,
            parent_id: None,
            project_name: None,
            project_id: None,
            note_id: None,
        };
        let raw: RawTaskRow = row.into();
        assert_eq!(raw.short_id, Some(42));
        assert!(raw.data.contains("description"));
        assert!(raw.data.contains("pending"));
        let _: serde_json::Value = serde_json::from_str(&raw.data).unwrap();
    }

    #[test]
    fn task_pg_row_to_raw_task_row_null_json() {
        let row = TaskPgRow {
            id: Uuid::nil(),
            short_id: None,
            data: serde_json::Value::Null,
            status: None,
            description: None,
            priority: None,
            entry_at: None,
            modified_at: None,
            due_at: None,
            scheduled_at: None,
            start_at: None,
            end_at: None,
            wait_at: None,
            parent_id: None,
            project_name: None,
            project_id: None,
            note_id: None,
        };
        let raw: RawTaskRow = row.into();
        assert_eq!(raw.data, "null");
        let _: serde_json::Value = serde_json::from_str(&raw.data).unwrap();
    }
}
