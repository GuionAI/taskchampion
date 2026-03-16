use crate::errors::Result;
use crate::storage::send_wrapper::Wrapper;
use crate::storage::{Storage, StorageTxn};
use async_trait::async_trait;
use std::path::Path;
use uuid::Uuid;

mod inner;
use inner::PowerSyncStorageInner;

/// PowerSyncStorage stores task data in a PowerSync-managed SQLite database.
///
/// PowerSync handles sync to a Postgres/Supabase backend transparently. This
/// storage layer reads and writes to the local SQLite file that PowerSync
/// maintains, using the same `send_wrapper` actor pattern as `SqliteStorage`.
pub struct PowerSyncStorage(Wrapper);

impl PowerSyncStorage {
    /// Open a PowerSync-managed database at `db_path` for the given `user_id`.
    ///
    /// The `tc_tasks` and `tc_operations` views must already exist (created by
    /// PowerSync from its schema definition). Local-only tables (`tc_sync_meta`)
    /// are created automatically.
    pub async fn new(db_path: &Path, user_id: Uuid) -> Result<Self> {
        let path = db_path.to_path_buf();
        Ok(Self(
            Wrapper::new(async move || PowerSyncStorageInner::new(&path, user_id)).await?,
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
    use crate::storage::TaskMap;

    async fn storage() -> Result<PowerSyncStorage> {
        Ok(PowerSyncStorage(
            Wrapper::new(async || PowerSyncStorageInner::new_for_test()).await?,
        ))
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
        task.insert("parent".into(), parent_uuid.to_string());

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
            got.get("parent").map(String::as_str),
            Some(parent_uuid.to_string().as_str())
        );
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
}
