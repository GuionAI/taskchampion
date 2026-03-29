use crate::errors::Result;
use crate::storage::send_wrapper::Wrapper;
use crate::storage::{Storage, StorageTxn};
use async_trait::async_trait;
use std::path::Path;

mod extension;
mod inner;
mod row_reader;
use inner::PowerSyncStorageInner;

/// PowerSyncStorage stores task data in a PowerSync-managed SQLite database.
///
/// PowerSync handles sync to a Postgres/Supabase backend transparently. This
/// storage layer reads and writes to the local SQLite file that PowerSync
/// maintains, using the same `send_wrapper` actor pattern as `SqliteStorage`.
pub struct PowerSyncStorage(Wrapper);

impl PowerSyncStorage {
    /// Open a PowerSync-managed database at `db_path`.
    ///
    /// The `tc_tasks` and `tc_operations` views must already exist (created by
    /// PowerSync from its schema definition). Local-only tables (`tc_sync_meta`)
    /// are created automatically.
    pub async fn new(db_path: &Path) -> Result<Self> {
        let path = db_path.to_path_buf();
        Ok(Self(
            Wrapper::new(async move || PowerSyncStorageInner::new(&path)).await?,
        ))
    }

    /// Create an in-memory PowerSyncStorage with all required tables for testing.
    #[cfg(any(test, feature = "test-utils"))]
    pub async fn new_for_test() -> Result<Self> {
        Ok(Self(
            Wrapper::new(async || PowerSyncStorageInner::new_for_test()).await?,
        ))
    }
}

#[async_trait]
impl Storage for PowerSyncStorage {
    async fn txn<'a>(&'a mut self) -> Result<Box<dyn StorageTxn + Send + 'a>> {
        Ok(self.0.txn().await?)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::errors::Result;
    use crate::storage::send_wrapper::{WrappedStorage, WrappedStorageTxn};
    use crate::storage::TaskMap;
    use inner::PowerSyncStorageInner;
    use uuid::Uuid;

    async fn storage() -> Result<PowerSyncStorage> {
        PowerSyncStorage::new_for_test().await
    }

    crate::storage::test::storage_tests_no_sync!(storage().await?);

    /// Verify that promoted string columns (status, description, priority, parent) survive
    /// a set_task / get_task round-trip via the dedicated columns, not just the JSON blob.
    #[tokio::test]
    async fn test_promoted_string_columns() -> Result<()> {
        let mut storage = storage().await?;
        let mut txn = storage.txn().await?;

        let uuid = Uuid::new_v4();
        let mut task: TaskMap = TaskMap::new();
        task.insert("status".into(), "pending".into());
        task.insert("description".into(), "round-trip test".into());
        task.insert("priority".into(), "H".into());
        let parent_uuid = Uuid::new_v4();
        task.insert("parent_id".into(), parent_uuid.to_string());
        task.insert("position".into(), "80".into());

        txn.set_task(uuid, task.clone()).await?;
        txn.commit().await?;
        drop(txn);

        let mut txn = storage.txn().await?;
        let got = txn.get_task(uuid).await?.expect("task should exist");
        assert_eq!(got.get("status").map(String::as_str), Some("pending"));
        assert_eq!(
            got.get("description").map(String::as_str),
            Some("round-trip test")
        );
        assert_eq!(got.get("priority").map(String::as_str), Some("H"));
        assert_eq!(
            got.get("parent_id").map(String::as_str),
            Some(parent_uuid.to_string().as_str())
        );
        assert_eq!(got.get("position").map(String::as_str), Some("80"));
        txn.commit().await?;
        Ok(())
    }

    /// Verify that all seven timestamp fields survive a set_task / get_task round-trip
    /// through epoch → ISO 8601 → epoch conversion.
    #[tokio::test]
    async fn test_timestamp_columns() -> Result<()> {
        let mut storage = storage().await?;
        let mut txn = storage.txn().await?;

        let uuid = Uuid::new_v4();
        let epoch = "1724612771";
        let mut task: TaskMap = TaskMap::new();
        for key in [
            "entry",
            "modified",
            "due",
            "scheduled",
            "start",
            "end",
            "wait",
        ] {
            task.insert(key.into(), epoch.into());
        }

        txn.set_task(uuid, task).await?;
        txn.commit().await?;
        drop(txn);

        let mut txn = storage.txn().await?;
        let got = txn.get_task(uuid).await?.expect("task should exist");
        for key in [
            "entry",
            "modified",
            "due",
            "scheduled",
            "start",
            "end",
            "wait",
        ] {
            assert_eq!(
                got.get(key).map(String::as_str),
                Some(epoch),
                "timestamp field {key:?} should round-trip cleanly"
            );
        }
        txn.commit().await?;
        Ok(())
    }

    /// Verify that tag_* keys round-trip through set_task/get_task via the data blob.
    #[tokio::test]
    async fn test_tags_round_trip() -> Result<()> {
        let mut storage = PowerSyncStorageInner::new_for_test()?;
        let uuid = Uuid::new_v4();

        let mut txn = storage.txn().await?;
        txn.create_task(uuid).await?;
        let mut task = TaskMap::new();
        task.insert("status".into(), "pending".into());
        task.insert("description".into(), "test task".into());
        task.insert("tag_work".into(), String::new());
        task.insert("tag_urgent".into(), String::new());
        txn.set_task(uuid, task).await?;
        txn.commit().await?;
        drop(txn);

        let mut txn = storage.txn().await?;
        let task = txn.get_task(uuid).await?.unwrap();
        assert_eq!(task.get("tag_work").map(String::as_str), Some(""));
        assert_eq!(task.get("tag_urgent").map(String::as_str), Some(""));
        txn.commit().await?;
        drop(txn);

        // Update: remove tag_urgent, add tag_next
        let mut txn = storage.txn().await?;
        let mut task = TaskMap::new();
        task.insert("status".into(), "pending".into());
        task.insert("description".into(), "test task".into());
        task.insert("tag_work".into(), String::new());
        task.insert("tag_next".into(), String::new());
        txn.set_task(uuid, task).await?;
        txn.commit().await?;
        drop(txn);

        let mut txn = storage.txn().await?;
        let task = txn.get_task(uuid).await?.unwrap();
        assert_eq!(task.get("tag_work").map(String::as_str), Some(""));
        assert_eq!(task.get("tag_next").map(String::as_str), Some(""));
        assert!(
            task.get("tag_urgent").is_none(),
            "tag_urgent should be removed"
        );
        txn.commit().await?;
        Ok(())
    }

    /// Verify that annotation_* keys round-trip through set_task/get_task via the data blob.
    #[tokio::test]
    async fn test_annotations_round_trip() -> Result<()> {
        let mut storage = PowerSyncStorageInner::new_for_test()?;
        let uuid = Uuid::new_v4();

        let mut txn = storage.txn().await?;
        txn.create_task(uuid).await?;
        let mut task = TaskMap::new();
        task.insert("status".into(), "pending".into());
        task.insert("description".into(), "test task".into());
        task.insert("annotation_1635301873".into(), "pick up groceries".into());
        task.insert("annotation_1635301883".into(), "left a message".into());
        txn.set_task(uuid, task).await?;
        txn.commit().await?;
        drop(txn);

        let mut txn = storage.txn().await?;
        let task = txn.get_task(uuid).await?.unwrap();
        assert_eq!(
            task.get("annotation_1635301873").map(String::as_str),
            Some("pick up groceries")
        );
        assert_eq!(
            task.get("annotation_1635301883").map(String::as_str),
            Some("left a message")
        );
        txn.commit().await?;
        Ok(())
    }

    /// Verify that tag_* and annotation_* keys are present in the raw `data` blob in tc_tasks.
    #[tokio::test]
    async fn test_data_blob_includes_tags_and_annotations() -> Result<()> {
        let mut storage = PowerSyncStorageInner::new_for_test()?;
        let uuid = Uuid::new_v4();

        {
            let mut txn = storage.txn().await?;
            txn.create_task(uuid).await?;
            let mut task = TaskMap::new();
            task.insert("status".into(), "pending".into());
            task.insert("tag_work".into(), String::new());
            task.insert("annotation_1635301873".into(), "note".into());
            task.insert("position".into(), "80".into());
            task.insert("dep_some-uuid".into(), String::new());
            task.insert("my_uda".into(), "custom value".into());
            txn.set_task(uuid, task).await?;
            txn.commit().await?;
        }

        // Query raw data column directly — confirms keys are persisted in the blob.
        let data_str: String = storage.conn.query_row(
            "SELECT data FROM tc_tasks WHERE id = ?",
            [&uuid.to_string()],
            |r| r.get(0),
        )?;
        let data_map: serde_json::Value = serde_json::from_str(&data_str)
            .map_err(|e| crate::errors::Error::Database(e.to_string()))?;
        let obj = data_map.as_object().unwrap();

        assert!(
            obj.keys().any(|k| k.starts_with("tag_")),
            "tag_* keys should be in data blob, got: {obj:?}"
        );
        assert!(
            obj.keys().any(|k| k.starts_with("annotation_")),
            "annotation_* keys should be in data blob, got: {obj:?}"
        );
        assert!(
            !obj.contains_key("position"),
            "position should not be in data blob, got: {obj:?}"
        );
        assert!(
            obj.contains_key("dep_some-uuid"),
            "dep_* should remain in data blob"
        );
        assert!(
            obj.contains_key("my_uda"),
            "UDAs should remain in data blob"
        );
        Ok(())
    }

    /// Verify that the project name round-trips: set_task stores the name in the
    /// projects table, and get_task JOINs it back into the returned TaskMap.
    #[tokio::test]
    async fn test_project_round_trip() -> Result<()> {
        let mut storage = storage().await?;
        let mut txn = storage.txn().await?;

        let uuid = Uuid::new_v4();
        let mut task: TaskMap = TaskMap::new();
        task.insert("project".into(), "home".into());

        txn.set_task(uuid, task).await?;
        txn.commit().await?;
        drop(txn);

        let mut txn = storage.txn().await?;
        let got = txn.get_task(uuid).await?.expect("task should exist");
        assert_eq!(got.get("project").map(String::as_str), Some("home"));

        // A second task with the same project name must reuse the same projects row.
        let uuid2 = Uuid::new_v4();
        let mut task2: TaskMap = TaskMap::new();
        task2.insert("project".into(), "home".into());
        txn.set_task(uuid2, task2).await?;
        txn.commit().await?;
        drop(txn);

        let mut txn = storage.txn().await?;
        let got2 = txn.get_task(uuid2).await?.expect("task2 should exist");
        assert_eq!(got2.get("project").map(String::as_str), Some("home"));
        txn.commit().await?;
        Ok(())
    }

    /// Verify that all_tasks merges tag_* and annotation_* from their side tables.
    /// Covers the bulk-read merge path independently of get_task.
    #[tokio::test]
    async fn test_all_tasks_includes_tags_and_annotations() -> Result<()> {
        let mut storage = PowerSyncStorageInner::new_for_test()?;
        let uuid = Uuid::new_v4();

        {
            let mut txn = storage.txn().await?;
            txn.create_task(uuid).await?;
            let mut task = TaskMap::new();
            task.insert("status".into(), "pending".into());
            task.insert("tag_home".into(), String::new());
            task.insert("annotation_1635301873".into(), "buy milk".into());
            txn.set_task(uuid, task).await?;
            txn.commit().await?;
        }

        let mut txn = storage.txn().await?;
        let all = txn.all_tasks().await?;
        txn.commit().await?;

        let (_, task_map) = all
            .into_iter()
            .find(|(u, _)| *u == uuid)
            .expect("task not found");
        assert_eq!(task_map.get("tag_home").map(String::as_str), Some(""));
        assert_eq!(
            task_map.get("annotation_1635301873").map(String::as_str),
            Some("buy milk")
        );
        Ok(())
    }

    /// Verify that all_operations() returns operations added via add_operation() in insertion
    /// order. This is the core fix — previously returned empty vec.
    #[tokio::test]
    async fn test_all_operations_round_trip() -> Result<()> {
        use crate::Operation;
        let mut storage = storage().await?;
        let uuid = uuid::Uuid::new_v4();
        let now = chrono::Utc::now();

        let mut txn = storage.txn().await?;
        txn.add_operation(Operation::Create { uuid }).await?;
        txn.add_operation(Operation::Update {
            uuid,
            property: "status".into(),
            value: Some("pending".into()),
            old_value: None,
            timestamp: now,
        })
        .await?;
        txn.add_operation(Operation::UndoPoint).await?;
        txn.commit().await?;
        drop(txn);

        let mut txn = storage.txn().await?;
        let ops = txn.all_operations().await?;
        assert_eq!(ops.len(), 3, "should return all 3 operations: {ops:?}");
        assert!(matches!(&ops[0], Operation::Create { .. }));
        assert!(matches!(&ops[1], Operation::Update { .. }));
        assert!(matches!(&ops[2], Operation::UndoPoint));
        txn.commit().await?;
        Ok(())
    }

    /// Verify that set_task returns an error for an annotation key with a non-integer epoch suffix
    /// rather than silently leaving it in the data blob.
    #[tokio::test]
    async fn test_set_task_rejects_invalid_annotation_epoch() -> Result<()> {
        let mut storage = PowerSyncStorageInner::new_for_test()?;
        let uuid = Uuid::new_v4();

        let mut txn = storage.txn().await?;
        txn.create_task(uuid).await?;
        let mut task = TaskMap::new();
        task.insert("annotation_not_an_epoch".into(), "oops".into());
        let result = txn.set_task(uuid, task).await;
        assert!(
            result.is_err(),
            "expected error for non-integer annotation epoch"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("epoch suffix is not an integer"),
            "unexpected error message: {msg}"
        );
        txn.commit().await?;
        Ok(())
    }

    /// Verify last-write-wins: when sync produces duplicate rows for the same
    /// tag name, get_tag_metadata returns the data from the row with the latest
    /// created_at timestamp (whole-row LWW, not per-field merge).
    #[tokio::test]
    async fn test_tag_metadata_lww() -> Result<()> {
        let mut storage = PowerSyncStorageInner::new_for_test()?;

        // Older row: has color + is_status + icon.
        storage.conn.execute(
            "INSERT INTO tc_tag_metadata (id, name, data, created_at) VALUES (?, ?, ?, ?)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                "work",
                r##"{"color":"#ff0000","is_status":true,"icon":7}"##,
                "2026-03-25 12:00:00.000"
            ],
        )?;
        // Newer row: only color (is_status/icon absent).
        storage.conn.execute(
            "INSERT INTO tc_tag_metadata (id, name, data, created_at) VALUES (?, ?, ?, ?)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                "work",
                r##"{"color":"#00ff00"}"##,
                "2026-03-26 12:00:00.000"
            ],
        )?;

        let mut txn = storage.txn().await?;
        let data = txn.get_tag_metadata("work".into()).await?;
        assert_eq!(
            data,
            Some(r##"{"color":"#00ff00"}"##.into()),
            "should return latest row's data (whole-row LWW, not per-field merge)"
        );
        txn.commit().await?;
        Ok(())
    }
}
