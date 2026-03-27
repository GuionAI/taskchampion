//! SQL string exports for PowerSync reactive watch integration.
//!
//! PowerSync's `db.watch()` uses `EXPLAIN` to detect which tables a query
//! touches. The strings exported here are **never executed** — they are passed
//! to `db.watch()` so PowerSync can set up the correct table-change listeners.

/// SQL that covers all task-related tables.
///
/// Pass this to `db.watch()` so PowerSync re-runs your query whenever any
/// task or project row changes.
#[uniffi::export(name = "allTaskTablesSql")]
pub fn all_task_tables_sql() -> String {
    "SELECT t.id, t.data, t.status, t.description, t.priority, \
            t.parent_id, t.position, \
            p.name \
     FROM tc_tasks t \
     LEFT JOIN projects p ON p.id = t.project_id"
        .to_string()
}

/// SQL that covers the tag colors table.
///
/// Pass this to `db.watch()` so PowerSync re-runs your query whenever a
/// `tc_tag_colors` row changes (e.g. color set on another device via sync).
#[uniffi::export(name = "tagColorTablesSql")]
pub fn tag_color_tables_sql() -> String {
    "SELECT id, name, color FROM tc_tag_colors".to_string()
}
