//! Storage-backed task tree operations on [`FfiSession`].

use taskchampion::{ExternalStorage, Operation, Operations, Replica, Status, TreeMap};
use uuid::Uuid;

use crate::replica_ops::{parse_uuid, FfiSession};
use crate::types::{FfiError, FfiTask};

#[derive(Clone, Copy)]
enum TreeStatusMutation {
    Complete,
    Delete,
}

impl TreeStatusMutation {
    fn parent_allowed(self, status: Status) -> bool {
        match self {
            Self::Complete => status == Status::Pending,
            Self::Delete => status != Status::Deleted,
        }
    }

    fn should_mutate_descendant(self, status: Status) -> bool {
        match self {
            Self::Complete => status == Status::Pending,
            Self::Delete => status != Status::Deleted,
        }
    }

    fn invalid_parent_message(self, parent_uuid: &str) -> String {
        match self {
            Self::Complete => format!("Task {parent_uuid} is not pending"),
            Self::Delete => format!("Task {parent_uuid} is already deleted"),
        }
    }

    fn apply(self, task: &mut taskchampion::Task, ops: &mut Operations) -> Result<(), FfiError> {
        if task.get_value("xstatus").is_some() {
            task.set_value("xstatus", None::<String>, ops)
                .map_err(FfiError::from)?;
        }

        match self {
            Self::Complete => task.done(ops).map_err(FfiError::from),
            Self::Delete => task
                .set_status(Status::Deleted, ops)
                .map_err(FfiError::from),
        }
    }
}

async fn mutate_tree_status(
    replica: &mut Replica<ExternalStorage>,
    parent_uuid: &str,
    mutation: TreeStatusMutation,
    dry_run: bool,
) -> Result<Vec<FfiTask>, FfiError> {
    let parent = parse_uuid(parent_uuid)?;
    let mut all_tasks = replica.all_tasks().await.map_err(FfiError::from)?;
    let tree = TreeMap::from_tasks(&all_tasks);

    let parent_task = all_tasks
        .get(&parent)
        .ok_or_else(|| FfiError::TaskNotFound {
            uuid: parent_uuid.to_string(),
        })?;
    if !mutation.parent_allowed(parent_task.get_status()) {
        return Err(FfiError::InvalidInput {
            message: mutation.invalid_parent_message(parent_uuid),
        });
    }

    let mut to_mutate = Vec::new();
    to_mutate.push(parent);
    to_mutate.extend(tree.descendants(parent).into_iter().filter(|uuid| {
        all_tasks
            .get(uuid)
            .is_some_and(|task| mutation.should_mutate_descendant(task.get_status()))
    }));

    if dry_run {
        return to_mutate
            .into_iter()
            .map(|uuid| {
                all_tasks
                    .get(&uuid)
                    .ok_or_else(|| FfiError::TaskNotFound {
                        uuid: uuid.to_string(),
                    })
                    .map(FfiTask::from)
            })
            .collect();
    }

    let mut ops = Operations::new();
    ops.push(Operation::UndoPoint);

    for uuid in &to_mutate {
        let task = all_tasks
            .get_mut(uuid)
            .ok_or_else(|| FfiError::TaskNotFound {
                uuid: uuid.to_string(),
            })?;
        mutation.apply(task, &mut ops)?;
    }

    replica
        .commit_operations(ops)
        .await
        .map_err(FfiError::from)?;

    replica.dependency_map(true).await.map_err(FfiError::from)?;

    let mut mutated = Vec::with_capacity(to_mutate.len());
    for uuid in to_mutate {
        let task = refetch_task(replica, uuid).await?;
        mutated.push(FfiTask::from(&task));
    }

    Ok(mutated)
}

async fn refetch_task(
    replica: &mut Replica<ExternalStorage>,
    uuid: Uuid,
) -> Result<taskchampion::Task, FfiError> {
    replica
        .get_task(uuid)
        .await
        .map_err(FfiError::from)?
        .ok_or_else(|| FfiError::Internal {
            message: format!("Task {uuid} missing after tree mutation"),
        })
}

#[uniffi::export]
impl FfiSession {
    /// Complete `parent_uuid` and all pending descendants in one operation group.
    ///
    /// Returns the tasks changed by this call. A single `undo()` call reverses the
    /// whole tree completion. If `dry_run` is `true`, returns the tasks that
    /// would be changed without writing anything; `None` defaults to `false`.
    pub async fn complete_tree(
        &self,
        parent_uuid: String,
        dry_run: Option<bool>,
    ) -> Result<Vec<FfiTask>, FfiError> {
        self.with_replica(|mut replica| async move {
            mutate_tree_status(
                &mut replica,
                &parent_uuid,
                TreeStatusMutation::Complete,
                dry_run.unwrap_or(false),
            )
            .await
        })
        .await
    }

    /// Soft-delete `parent_uuid` and all non-deleted descendants in one operation group.
    ///
    /// Returns the tasks changed by this call. A single `undo()` call reverses the
    /// whole tree deletion. If `dry_run` is `true`, returns the tasks that would
    /// be changed without writing anything; `None` defaults to `false`.
    pub async fn delete_tree(
        &self,
        parent_uuid: String,
        dry_run: Option<bool>,
    ) -> Result<Vec<FfiTask>, FfiError> {
        self.with_replica(|mut replica| async move {
            mutate_tree_status(
                &mut replica,
                &parent_uuid,
                TreeStatusMutation::Delete,
                dry_run.unwrap_or(false),
            )
            .await
        })
        .await
    }
}
