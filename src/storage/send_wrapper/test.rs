use super::Wrapper;
use crate::errors::{Error, Result};
use crate::operation::Operation;
use crate::storage::inmemory::InMemoryStorage;
use crate::storage::send_wrapper::{WrappedStorage, WrappedStorageTxn};
use crate::storage::{Storage, StorageTxn, TaskMap};
use async_trait::async_trait;
use pretty_assertions::assert_eq;
use uuid::Uuid;

// Implement WrappedStorage(Txn) for InMemoryStorage and a boxed StorageTxn

#[async_trait(?Send)]
impl WrappedStorageTxn for Box<dyn StorageTxn + Send + '_> {
    async fn get_task(&mut self, uuid: Uuid) -> Result<Option<TaskMap>> {
        self.as_mut().get_task(uuid).await
    }

    async fn get_pending_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        self.as_mut().get_pending_tasks().await
    }

    async fn create_task(&mut self, uuid: Uuid) -> Result<bool> {
        self.as_mut().create_task(uuid).await
    }

    async fn set_task(&mut self, uuid: Uuid, task: TaskMap) -> Result<()> {
        self.as_mut().set_task(uuid, task).await
    }

    async fn delete_task(&mut self, uuid: Uuid) -> Result<bool> {
        self.as_mut().delete_task(uuid).await
    }

    async fn all_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        self.as_mut().all_tasks().await
    }

    async fn all_task_uuids(&mut self) -> Result<Vec<Uuid>> {
        self.as_mut().all_task_uuids().await
    }

    async fn get_task_operations(&mut self, uuid: Uuid) -> Result<Vec<Operation>> {
        self.as_mut().get_task_operations(uuid).await
    }

    async fn unsynced_operations(&mut self) -> Result<Vec<Operation>> {
        self.as_mut().unsynced_operations().await
    }

    async fn add_operation(&mut self, op: Operation) -> Result<()> {
        self.as_mut().add_operation(op).await
    }

    async fn remove_operation(&mut self, op: Operation) -> Result<()> {
        self.as_mut().remove_operation(op).await
    }

    #[allow(clippy::wrong_self_convention)] // mut is required here for storage access
    async fn is_empty(&mut self) -> Result<bool> {
        self.as_mut().is_empty().await
    }

    async fn commit(&mut self) -> Result<()> {
        self.as_mut().commit().await
    }
}

#[async_trait(?Send)]
impl WrappedStorage for InMemoryStorage {
    async fn txn<'a>(&'a mut self) -> Result<Box<dyn WrappedStorageTxn + 'a>> {
        Ok(Box::new(<InMemoryStorage as Storage>::txn(self).await?))
    }
}

async fn storage() -> Result<Wrapper> {
    Wrapper::new(async || Ok(InMemoryStorage::new())).await
}

crate::storage::test::storage_tests_no_sync!(storage().await.unwrap());

#[tokio::test]
async fn test_implicit_rollback() -> Result<()> {
    let mut storage = storage().await?;
    let uuid = Uuid::new_v4();

    // Begin a transaction, create a task, but do not commit.
    // The transaction will go out of scope, triggering Drop.
    {
        let mut txn = storage.txn().await?;
        assert!(txn.create_task(uuid).await?);
        // txn is dropped here, which should trigger a rollback message.
    }

    // Begin a new transaction and verify the task does not exist.
    let mut txn = storage.txn().await?;
    let task = txn.get_task(uuid).await?;
    assert_eq!(task, None, "Task should not exist after implicit rollback");

    Ok(())
}

#[tokio::test]
async fn test_init_failure() -> Result<()> {
    // The constructor runs in the thread, and its failure must be transmitted back via channels to
    // appear here.
    assert!(
        Wrapper::new::<InMemoryStorage, _, _>(async || Err(Error::Database("uhoh!".into())))
            .await
            .is_err()
    );
    Ok(())
}
