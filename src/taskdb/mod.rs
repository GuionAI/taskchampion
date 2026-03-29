use crate::errors::Result;
#[cfg(test)]
use crate::operation::Operation;
use crate::storage::{Storage, TaskMap};
use crate::Operations;
use uuid::Uuid;

mod apply;
pub(crate) mod op;
pub(crate) mod undo;

/// A TaskDb is the backend for a replica.  It manages the storage, operations, synchronization,
/// and so on, and all the invariants that come with it.  It leaves the meaning of particular task
/// properties to the replica and task implementations.
pub(crate) struct TaskDb<S: Storage> {
    storage: S,
}

impl<S: Storage> TaskDb<S> {
    /// Create a new TaskDb with the given backend storage
    pub(crate) fn new(storage: S) -> TaskDb<S> {
        TaskDb { storage }
    }

    /// Apply `operations` to the database in a single transaction.
    ///
    /// The operations will be appended to the list of local operations, and the set of tasks will
    /// be updated accordingly.
    pub(crate) async fn commit_operations(&mut self, operations: Operations) -> Result<()> {
        let mut txn = self.storage.txn().await?;
        apply::apply_operations(txn.as_mut(), &operations).await?;

        for operation in operations {
            txn.add_operation(operation).await?;
        }

        txn.commit().await
    }

    /// Get all tasks.
    pub(crate) async fn all_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let mut txn = self.storage.txn().await?;
        txn.all_tasks().await
    }

    /// Get the UUIDs of all tasks
    pub(crate) async fn all_task_uuids(&mut self) -> Result<Vec<Uuid>> {
        let mut txn = self.storage.txn().await?;
        txn.all_task_uuids().await
    }

    /// Get a single task, by uuid.
    pub(crate) async fn get_task(&mut self, uuid: Uuid) -> Result<Option<TaskMap>> {
        let mut txn = self.storage.txn().await?;
        txn.get_task(uuid).await
    }

    /// Get all pending tasks from the working set
    pub(crate) async fn get_pending_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let mut txn = self.storage.txn().await?;
        txn.get_pending_tasks().await
    }

    pub(crate) async fn get_task_operations(&mut self, uuid: Uuid) -> Result<Operations> {
        let mut txn = self.storage.txn().await?;
        txn.get_task_operations(uuid).await
    }

    /// Get the number of operations in storage, excluding undo points.
    pub(crate) async fn num_operations(&mut self) -> Result<usize> {
        let mut txn = self.storage.txn().await?;
        Ok(txn
            .all_operations()
            .await?
            .iter()
            .filter(|o| !o.is_undo_point())
            .count())
    }

    /// Get the number of undo points in storage.
    pub(crate) async fn num_undo_points(&mut self) -> Result<usize> {
        let mut txn = self.storage.txn().await?;
        Ok(txn
            .all_operations()
            .await?
            .iter()
            .filter(|o| o.is_undo_point())
            .count())
    }

    /// Return the operations back to and including the last undo point, or since the last sync if
    /// no undo point is found.
    ///
    /// The operations are returned in the order they were applied. Use
    /// [`commit_reversed_operations`] to "undo" them.
    pub(crate) async fn get_undo_operations(&mut self) -> Result<Operations> {
        let mut txn = self.storage.txn().await?;
        undo::get_undo_operations(txn.as_mut()).await
    }

    /// Commit the reverse of the given operations, beginning with the last operation in the given
    /// operations and proceeding to the first.
    ///
    /// This method only supports reversing operations if they precisely match local operations
    /// that have not yet been synchronized, and will return `false` if this is not the case.
    pub(crate) async fn commit_reversed_operations(
        &mut self,
        undo_ops: Operations,
    ) -> Result<bool> {
        let mut txn = self.storage.txn().await?;
        undo::commit_reversed_operations(txn.as_mut(), undo_ops).await
    }

    /// Get the metadata JSON for a tag by name.
    pub(crate) async fn get_tag_metadata(&mut self, name: String) -> Result<Option<String>> {
        // Read-only: transaction is intentionally not committed
        let mut txn = self.storage.txn().await?;
        txn.get_tag_metadata(name).await
    }

    /// Set the metadata JSON for a tag by name, committing immediately.
    pub(crate) async fn set_tag_metadata(&mut self, name: String, data: String) -> Result<()> {
        let mut txn = self.storage.txn().await?;
        txn.set_tag_metadata(name, data).await?;
        txn.commit().await
    }

    /// Get all unique tag names across all tasks.
    pub(crate) async fn get_all_tags(&mut self) -> Result<Vec<String>> {
        // Read-only: transaction is intentionally not committed
        let mut txn = self.storage.txn().await?;
        txn.get_all_tags().await
    }

    // functions for supporting tests

    #[cfg(test)]
    pub(crate) async fn sorted_tasks(&mut self) -> Vec<(Uuid, Vec<(String, String)>)> {
        let mut res: Vec<(Uuid, Vec<(String, String)>)> = self
            .all_tasks()
            .await
            .unwrap()
            .iter()
            .map(|(u, t)| {
                let mut t = t
                    .iter()
                    .map(|(p, v)| (p.clone(), v.clone()))
                    .collect::<Vec<(String, String)>>();
                t.sort();
                (*u, t)
            })
            .collect();
        res.sort();
        res
    }

    #[cfg(test)]
    pub(crate) async fn operations(&mut self) -> Vec<Operation> {
        let mut txn = self.storage.txn().await.unwrap();
        txn.all_operations().await.unwrap().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::inmemory::InMemoryStorage;

    use super::*;
    use chrono::Utc;
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    #[tokio::test]
    async fn commit_operations() -> Result<()> {
        let mut db = TaskDb::new(InMemoryStorage::new());
        let uuid = Uuid::new_v4();
        let now = Utc::now();
        let mut ops = Operations::new();
        ops.push(Operation::Create { uuid });
        ops.push(Operation::Update {
            uuid,
            property: String::from("title"),
            value: Some("my task".into()),
            timestamp: now,
            old_value: Some("old".into()),
        });

        db.commit_operations(ops).await?;

        assert_eq!(
            db.sorted_tasks().await,
            vec![(uuid, vec![("title".into(), "my task".into())])]
        );
        assert_eq!(
            db.operations().await,
            vec![
                Operation::Create { uuid },
                Operation::Update {
                    uuid,
                    property: String::from("title"),
                    value: Some("my task".into()),
                    timestamp: now,
                    old_value: Some("old".into()),
                },
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_num_operations() {
        let mut db = TaskDb::new(InMemoryStorage::new());
        let mut ops = Operations::new();
        ops.push(Operation::Create {
            uuid: Uuid::new_v4(),
        });
        ops.push(Operation::UndoPoint);
        ops.push(Operation::Create {
            uuid: Uuid::new_v4(),
        });
        db.commit_operations(ops).await.unwrap();
        assert_eq!(db.num_operations().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_num_undo_points() {
        let mut db = TaskDb::new(InMemoryStorage::new());
        let mut ops = Operations::new();
        ops.push(Operation::UndoPoint);
        db.commit_operations(ops).await.unwrap();
        assert_eq!(db.num_undo_points().await.unwrap(), 1);

        let mut ops = Operations::new();
        ops.push(Operation::UndoPoint);
        db.commit_operations(ops).await.unwrap();
        assert_eq!(db.num_undo_points().await.unwrap(), 2);
    }
}
