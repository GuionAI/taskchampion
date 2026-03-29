#![allow(clippy::new_without_default)]

use crate::errors::{Error, Result};
use crate::operation::Operation;
use crate::storage::{Storage, StorageTxn, TaskMap};
use async_trait::async_trait;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(PartialEq, Debug, Clone)]
struct Data {
    tasks: HashMap<Uuid, TaskMap>,
    operations: Vec<Operation>,
    tag_metadata: HashMap<String, String>,
}

struct Txn<'t> {
    storage: &'t mut InMemoryStorage,
    new_data: Option<Data>,
}

impl Txn<'_> {
    fn mut_data_ref(&mut self) -> &mut Data {
        if self.new_data.is_none() {
            self.new_data = Some(self.storage.data.clone());
        }
        if let Some(ref mut data) = self.new_data {
            data
        } else {
            unreachable!();
        }
    }

    fn data_ref(&mut self) -> &Data {
        if let Some(ref data) = self.new_data {
            data
        } else {
            &self.storage.data
        }
    }
}

#[async_trait]
impl StorageTxn for Txn<'_> {
    async fn get_task(&mut self, uuid: Uuid) -> Result<Option<TaskMap>> {
        match self.data_ref().tasks.get(&uuid) {
            None => Ok(None),
            Some(t) => Ok(Some(t.clone())),
        }
    }

    async fn get_pending_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        let res = self
            .data_ref()
            .tasks
            .iter()
            .filter(|(_, taskmap)| {
                taskmap
                    .get("status")
                    .map(|s| s == "pending")
                    .unwrap_or(false)
            })
            .map(|(uuid, taskmap)| (*uuid, taskmap.clone()))
            .collect();
        Ok(res)
    }

    async fn create_task(&mut self, uuid: Uuid) -> Result<bool> {
        if let ent @ Entry::Vacant(_) = self.mut_data_ref().tasks.entry(uuid) {
            ent.or_insert_with(TaskMap::new);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn set_task(&mut self, uuid: Uuid, task: TaskMap) -> Result<()> {
        self.mut_data_ref().tasks.insert(uuid, task);
        Ok(())
    }

    async fn delete_task(&mut self, uuid: Uuid) -> Result<bool> {
        Ok(self.mut_data_ref().tasks.remove(&uuid).is_some())
    }

    async fn all_tasks(&mut self) -> Result<Vec<(Uuid, TaskMap)>> {
        Ok(self
            .data_ref()
            .tasks
            .iter()
            .map(|(u, t)| (*u, t.clone()))
            .collect())
    }

    async fn all_task_uuids(&mut self) -> Result<Vec<Uuid>> {
        Ok(self.data_ref().tasks.keys().copied().collect())
    }

    async fn get_task_operations(&mut self, uuid: Uuid) -> Result<Vec<Operation>> {
        Ok(self
            .data_ref()
            .operations
            .iter()
            .filter(|op| op.get_uuid() == Some(uuid))
            .cloned()
            .collect())
    }

    async fn all_operations(&mut self) -> Result<Vec<Operation>> {
        Ok(self.data_ref().operations.clone())
    }

    async fn add_operation(&mut self, op: Operation) -> Result<()> {
        self.mut_data_ref().operations.push(op);
        Ok(())
    }

    async fn remove_operation(&mut self, op: Operation) -> Result<()> {
        if let Some(last_op) = self.data_ref().operations.last() {
            if last_op == &op {
                self.mut_data_ref().operations.pop();
                return Ok(());
            }
        }
        Err(Error::Database(
            "Last operation does not match -- cannot remove".to_string(),
        ))
    }

    async fn get_tag_metadata(&mut self, name: String) -> Result<Option<String>> {
        Ok(self.data_ref().tag_metadata.get(&name).cloned())
    }

    async fn set_tag_metadata(&mut self, name: String, data: String) -> Result<()> {
        self.mut_data_ref().tag_metadata.insert(name, data);
        Ok(())
    }

    async fn get_all_tags(&mut self) -> Result<Vec<String>> {
        let tags: Vec<String> = self
            .data_ref()
            .tasks
            .values()
            .flat_map(|task| {
                task.keys()
                    .filter(|k| k.starts_with("tag_"))
                    .filter_map(|k| k.strip_prefix("tag_").map(String::from))
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(tags)
    }

    async fn commit(&mut self) -> Result<()> {
        // copy the new_data back into storage to commit the transaction
        if let Some(data) = self.new_data.take() {
            self.storage.data = data;
        }
        Ok(())
    }
}

/// InMemoryStorage is a simple in-memory task storage implementation.  It is not useful for
/// production data, but is useful for testing purposes.
#[derive(PartialEq, Debug, Clone)]
pub struct InMemoryStorage {
    data: Data,
}

impl InMemoryStorage {
    pub fn new() -> InMemoryStorage {
        InMemoryStorage {
            data: Data {
                tasks: HashMap::new(),
                operations: vec![],
                tag_metadata: HashMap::new(),
            },
        }
    }
}

#[async_trait]
impl Storage for InMemoryStorage {
    async fn txn<'a>(&'a mut self) -> Result<Box<dyn StorageTxn + Send + 'a>> {
        Ok(Box::new(Txn {
            storage: self,
            new_data: None,
        }))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    async fn storage() -> InMemoryStorage {
        InMemoryStorage::new()
    }

    crate::storage::test::storage_tests_no_sync!(storage().await);
}
