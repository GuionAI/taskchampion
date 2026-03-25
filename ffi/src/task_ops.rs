//! Task mutation methods on [`FfiSession`].

use chrono::DateTime;
use taskchampion::{Annotation, Operation, Operations, Status, Tag};

use crate::replica_ops::{parse_uuid, FfiSession};
use crate::types::{FfiError, FfiTask, TaskMutation};

#[uniffi::export]
impl FfiSession {
    /// Apply a batch of mutations to a task in a single transaction.
    ///
    /// All mutations share one undo point — a single `undo()` call will reverse
    /// the entire batch.
    ///
    /// Returns the updated task, or `None` if the task no longer exists after the
    /// mutations (defensive; should not happen via normal mutations).
    pub async fn mutate_task(
        &self,
        uuid: String,
        mutations: Vec<TaskMutation>,
    ) -> Result<Option<FfiTask>, FfiError> {
        self.with_replica(|mut replica| async move {
            let task_uuid = parse_uuid(&uuid)?;
            let mut task = replica
                .get_task(task_uuid)
                .await
                .map_err(FfiError::from)?
                .ok_or_else(|| FfiError::TaskNotFound {
                    uuid: uuid.clone(),
                })?;

            let mut ops = Operations::new();
            ops.push(Operation::UndoPoint);

            for mutation in mutations {
                apply_mutation(&mut task, mutation, &mut ops)?;
            }

            replica
                .commit_operations(ops)
                .await
                .map_err(FfiError::from)?;

            // Re-fetch — may be `None` if the task was hard-deleted (defensive).
            let updated = replica.get_task(task_uuid).await.map_err(FfiError::from)?;
            Ok(updated.as_ref().map(FfiTask::from))
        })
        .await
    }
}

fn apply_mutation(
    task: &mut taskchampion::Task,
    mutation: TaskMutation,
    ops: &mut Operations,
) -> Result<(), FfiError> {
    match mutation {
        TaskMutation::SetDescription { value } => {
            task.set_description(value, ops).map_err(FfiError::from)?;
        }
        TaskMutation::SetStatus { status } => {
            task.set_status(Status::from(status), ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetPriority { value } => {
            task.set_priority(value, ops).map_err(FfiError::from)?;
        }
        TaskMutation::SetDue { epoch } => {
            // FFI receives i64 epoch; set_timestamp expects DateTime<Utc>. Both
            // paths store identical epoch-second strings in the task map.
            let value = epoch.map(|e| e.to_string());
            task.set_value("due", value, ops).map_err(FfiError::from)?;
        }
        TaskMutation::SetWait { epoch } => {
            let value = epoch.map(|e| e.to_string());
            task.set_value("wait", value, ops).map_err(FfiError::from)?;
        }
        TaskMutation::SetEntry { epoch } => {
            let value = epoch.map(|e| e.to_string());
            task.set_value("entry", value, ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetParent { uuid } => {
            let parent = uuid.map(|u| parse_uuid(&u)).transpose()?;
            task.set_parent(parent, ops).map_err(FfiError::from)?;
        }
        TaskMutation::SetPosition { value } => {
            task.set_position(value, ops).map_err(FfiError::from)?;
        }
        TaskMutation::AddTag { tag } => {
            let tag: Tag = tag.as_str().try_into().map_err(|e| FfiError::InvalidInput {
                message: format!("Invalid tag: {e}"),
            })?;
            task.add_tag(&tag, ops).map_err(FfiError::from)?;
        }
        TaskMutation::RemoveTag { tag } => {
            let tag: Tag = tag.as_str().try_into().map_err(|e| FfiError::InvalidInput {
                message: format!("Invalid tag: {e}"),
            })?;
            task.remove_tag(&tag, ops).map_err(FfiError::from)?;
        }
        TaskMutation::AddAnnotation { entry, description } => {
            let ann = Annotation {
                entry: DateTime::from_timestamp(entry, 0).ok_or_else(|| FfiError::InvalidInput {
                    message: format!("Invalid epoch: {entry}"),
                })?,
                description,
            };
            task.add_annotation(ann, ops).map_err(FfiError::from)?;
        }
        TaskMutation::RemoveAnnotation { entry } => {
            let ts = DateTime::from_timestamp(entry, 0).ok_or_else(|| FfiError::InvalidInput {
                message: format!("Invalid epoch: {entry}"),
            })?;
            task.remove_annotation(ts, ops).map_err(FfiError::from)?;
        }
        TaskMutation::AddDependency { uuid } => {
            let dep = parse_uuid(&uuid)?;
            task.add_dependency(dep, ops).map_err(FfiError::from)?;
        }
        TaskMutation::RemoveDependency { uuid } => {
            let dep = parse_uuid(&uuid)?;
            task.remove_dependency(dep, ops).map_err(FfiError::from)?;
        }
        TaskMutation::Done => {
            task.done(ops).map_err(FfiError::from)?;
        }
        TaskMutation::Start => {
            task.start(ops).map_err(FfiError::from)?;
        }
        TaskMutation::Stop => {
            task.stop(ops).map_err(FfiError::from)?;
        }
        TaskMutation::Delete => {
            // Soft delete: sets status to `Deleted`. The task still exists and
            // can be re-fetched with `get_task`.
            task.set_status(Status::Deleted, ops)
                .map_err(FfiError::from)?;
        }
    }
    Ok(())
}
