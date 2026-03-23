use anyhow::Context;
use chrono::DateTime;
use uuid::Uuid;

use crate::errors::{Error, Result};
use crate::storage::TaskMap;

/// Convert Unix epoch string (e.g. "1724612771") to ISO 8601 string.
fn epoch_to_iso(epoch_str: &str) -> Option<String> {
    let secs: i64 = epoch_str.parse().ok()?;
    let dt = DateTime::from_timestamp(secs, 0)?;
    Some(dt.to_rfc3339())
}

/// Convert ISO 8601 string back to Unix epoch string.
fn iso_to_epoch(iso_str: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(iso_str).ok()?;
    Some(dt.timestamp().to_string())
}

/// Remove a timestamp field from `task_data`, converting epoch → ISO 8601.
/// Returns `Err` if the value is present but not a valid epoch integer.
pub(super) fn extract_timestamp(task_data: &mut TaskMap, key: &str) -> Result<Option<String>> {
    match task_data.remove(key) {
        None => Ok(None),
        Some(epoch_str) => match epoch_to_iso(&epoch_str) {
            Some(iso) => Ok(Some(iso)),
            None => Err(Error::Database(format!(
                "Invalid epoch timestamp for field {key:?}: {epoch_str:?}"
            ))),
        },
    }
}

/// Raw row fetched from tc_tasks JOIN projects.
pub(super) struct RawTaskRow {
    pub(super) id: String,
    pub(super) data: String,
    pub(super) status: Option<String>,
    pub(super) description: Option<String>,
    pub(super) priority: Option<String>,
    pub(super) entry_at: Option<String>,
    pub(super) modified_at: Option<String>,
    pub(super) due_at: Option<String>,
    pub(super) scheduled_at: Option<String>,
    pub(super) start_at: Option<String>,
    pub(super) end_at: Option<String>,
    pub(super) wait_at: Option<String>,
    pub(super) parent_id: Option<String>,
    pub(super) position: Option<String>,
    pub(super) project_name: Option<String>,
}

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

pub(super) fn raw_to_task(raw: RawTaskRow) -> Result<(Uuid, TaskMap)> {
    let uuid = Uuid::parse_str(&raw.id)
        .map_err(|e| Error::Database(format!("Invalid UUID in tc_tasks: {e}")))?;
    let mut task_map: TaskMap = serde_json::from_str(&raw.data)
        .map_err(|e| Error::Database(format!("Failed to parse task data for task {uuid}: {e}")))?;

    // Inject string columns back into the task map.
    if let Some(v) = raw.status {
        task_map.insert("status".into(), v);
    }
    if let Some(v) = raw.description {
        task_map.insert("description".into(), v);
    }
    if let Some(v) = raw.priority {
        task_map.insert("priority".into(), v);
    }
    if let Some(v) = raw.parent_id {
        task_map.insert("parent_id".into(), v);
    }
    if let Some(v) = raw.position {
        task_map.insert("position".into(), v);
    }
    if let Some(v) = raw.project_name {
        task_map.insert("project".into(), v);
    }

    // Inject timestamp columns (ISO 8601 → epoch string) back into the task map.
    // An ISO value present in the DB but failing to parse is a data integrity error.
    for (col_val, taskmap_key) in [
        (raw.entry_at, "entry"),
        (raw.modified_at, "modified"),
        (raw.due_at, "due"),
        (raw.scheduled_at, "scheduled"),
        (raw.start_at, "start"),
        (raw.end_at, "end"),
        (raw.wait_at, "wait"),
    ] {
        if let Some(iso) = col_val {
            let epoch = iso_to_epoch(&iso).ok_or_else(|| {
                Error::Database(format!(
                    "Malformed ISO timestamp in column {taskmap_key}_at for task {uuid}: {iso:?}"
                ))
            })?;
            task_map.insert(taskmap_key.into(), epoch);
        }
    }

    Ok((uuid, task_map))
}

/// Shared column projection for all tc_tasks queries (requires `t` and `p` aliases).
pub(super) const TASK_SELECT_COLS: &str = "t.id, t.data, t.status, t.description, t.priority, \
    t.entry_at, t.modified_at, t.due_at, t.scheduled_at, \
    t.start_at, t.end_at, t.wait_at, t.parent_id, t.position, p.name as project_name";

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::TaskMap;
    use uuid::Uuid;

    #[test]
    fn epoch_to_iso_valid() {
        let result = epoch_to_iso("1724612771");
        assert!(result.is_some());
        let iso = result.unwrap();
        assert!(
            iso.starts_with("2024-08-25"),
            "expected 2024-08-25 date, got {iso}"
        );
    }

    #[test]
    fn epoch_to_iso_invalid() {
        assert_eq!(epoch_to_iso("not_a_number"), None);
    }

    #[test]
    fn epoch_to_iso_empty() {
        assert_eq!(epoch_to_iso(""), None);
    }

    #[test]
    fn iso_to_epoch_valid() {
        let result = iso_to_epoch("2024-08-25T19:06:11+00:00");
        assert_eq!(result, Some("1724612771".to_string()));
    }

    #[test]
    fn iso_to_epoch_invalid() {
        assert_eq!(iso_to_epoch("not-an-iso"), None);
    }

    #[test]
    fn epoch_iso_round_trip() {
        let epoch = "1724612771";
        let iso = epoch_to_iso(epoch).unwrap();
        let back = iso_to_epoch(&iso).unwrap();
        assert_eq!(back, epoch);
    }

    #[test]
    fn extract_timestamp_present_valid() {
        let mut map = TaskMap::new();
        map.insert("entry".into(), "1724612771".into());
        let result = extract_timestamp(&mut map, "entry").unwrap();
        let iso = result.expect("should return Some ISO string");
        assert_eq!(iso, epoch_to_iso("1724612771").unwrap());
        assert!(
            !map.contains_key("entry"),
            "key should be removed after extraction"
        );
    }

    #[test]
    fn extract_timestamp_absent() {
        let mut map = TaskMap::new();
        let result = extract_timestamp(&mut map, "entry").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn extract_timestamp_invalid() {
        let mut map = TaskMap::new();
        map.insert("entry".into(), "not_a_timestamp".into());
        let result = extract_timestamp(&mut map, "entry");
        assert!(result.is_err());
    }

    fn make_empty_raw(uuid: &Uuid) -> RawTaskRow {
        RawTaskRow {
            id: uuid.to_string(),
            data: "{}".to_string(),
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
            position: None,
            project_name: None,
        }
    }

    #[test]
    fn raw_to_task_basic() {
        let uuid = Uuid::new_v4();
        let raw = make_empty_raw(&uuid);
        let (got_uuid, task_map) = raw_to_task(raw).unwrap();
        assert_eq!(got_uuid, uuid);
        assert!(task_map.is_empty());
    }

    #[test]
    fn raw_to_task_string_columns() {
        let uuid = Uuid::new_v4();
        let mut raw = make_empty_raw(&uuid);
        raw.status = Some("pending".into());
        raw.description = Some("my task".into());
        raw.priority = Some("H".into());
        raw.parent_id = Some("parent-uuid".into());
        raw.position = Some("80".into());
        raw.project_name = Some("work".into());
        let (_, task_map) = raw_to_task(raw).unwrap();
        assert_eq!(task_map.get("status").map(String::as_str), Some("pending"));
        assert_eq!(
            task_map.get("description").map(String::as_str),
            Some("my task")
        );
        assert_eq!(task_map.get("priority").map(String::as_str), Some("H"));
        assert_eq!(
            task_map.get("parent_id").map(String::as_str),
            Some("parent-uuid")
        );
        assert_eq!(task_map.get("position").map(String::as_str), Some("80"));
        assert_eq!(task_map.get("project").map(String::as_str), Some("work"));
    }

    #[test]
    fn raw_to_task_timestamps() {
        let uuid = Uuid::new_v4();
        let iso = epoch_to_iso("1724612771").unwrap();
        let mut raw = make_empty_raw(&uuid);
        raw.entry_at = Some(iso.clone());
        raw.modified_at = Some(iso.clone());
        let (_, task_map) = raw_to_task(raw).unwrap();
        assert_eq!(
            task_map.get("entry").map(String::as_str),
            Some("1724612771")
        );
        assert_eq!(
            task_map.get("modified").map(String::as_str),
            Some("1724612771")
        );
    }

    #[test]
    fn raw_to_task_invalid_uuid() {
        let mut raw = make_empty_raw(&Uuid::new_v4());
        raw.id = "not-a-uuid".to_string();
        assert!(raw_to_task(raw).is_err());
    }

    #[test]
    fn raw_to_task_invalid_data_json() {
        let uuid = Uuid::new_v4();
        let mut raw = make_empty_raw(&uuid);
        raw.data = "not json".to_string();
        assert!(raw_to_task(raw).is_err());
    }

    #[test]
    fn raw_to_task_invalid_iso_timestamp() {
        let uuid = Uuid::new_v4();
        let mut raw = make_empty_raw(&uuid);
        raw.entry_at = Some("not-an-iso".to_string());
        assert!(raw_to_task(raw).is_err());
    }
}
