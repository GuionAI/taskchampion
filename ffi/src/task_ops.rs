//! Task mutation methods on [`FfiSession`].

use chrono::DateTime;
use taskchampion::{Annotation, Operation, Operations, Status, Tag};

use crate::replica_ops::{parse_uuid, FfiSession};
use crate::types::{FfiError, FfiTask, TaskMutation, DEDICATED_UDA_FIELDS};

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
                .ok_or_else(|| FfiError::TaskNotFound { uuid: uuid.clone() })?;

            // Pre-validate all AddTag mutations against tc_config (one config read per batch).
            // Collect raw name strings from AddTag variants for validation.
            let add_tag_names: Vec<&str> = mutations
                .iter()
                .filter_map(|m| {
                    if let TaskMutation::AddTag { tag } = m {
                        Some(tag.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            if !add_tag_names.is_empty() {
                let config = replica
                    .get_tc_config_parsed()
                    .await
                    .map_err(FfiError::from)?;
                for name in add_tag_names {
                    // Validate tag format first — a malformed name returns
                    // InvalidInput (matching apply_mutation's behaviour),
                    // not TagNotFound.
                    let tag: Tag = name.try_into().map_err(|e| FfiError::InvalidInput {
                        message: format!("Invalid tag: {e}"),
                    })?;
                    // Synthetic tags (e.g. "WAITING") cannot be user-managed.
                    if tag.is_synthetic() {
                        return Err(FfiError::InvalidInput {
                            message: format!("'{name}' is a synthetic tag and cannot be added"),
                        });
                    }
                    if !config.has_tag(name) {
                        return Err(FfiError::TagNotFound {
                            name: name.to_string(),
                        });
                    }
                }
            }

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

/// Clear the `xstatus` UDA if it is currently set.
///
/// Called from `SetStatus`, `Done`, and `Delete` arms to ensure xstatus is
/// never left set on a non-pending task.
fn clear_xstatus_if_set(
    task: &mut taskchampion::Task,
    ops: &mut Operations,
) -> Result<(), FfiError> {
    if task.get_value("xstatus").is_some() {
        task.set_value("xstatus", None::<String>, ops)
            .map_err(FfiError::from)?;
    }
    Ok(())
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
            let new_status = Status::from(status);
            // Auto-clear xstatus when transitioning to non-pending status.
            if new_status != Status::Pending {
                clear_xstatus_if_set(task, ops)?;
            }
            task.set_status(new_status, ops).map_err(FfiError::from)?;
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
            let tag: Tag = tag
                .as_str()
                .try_into()
                .map_err(|e| FfiError::InvalidInput {
                    message: format!("Invalid tag: {e}"),
                })?;
            task.add_tag(&tag, ops).map_err(FfiError::from)?;
        }
        TaskMutation::RemoveTag { tag } => {
            let tag: Tag = tag
                .as_str()
                .try_into()
                .map_err(|e| FfiError::InvalidInput {
                    message: format!("Invalid tag: {e}"),
                })?;
            task.remove_tag(&tag, ops).map_err(FfiError::from)?;
        }
        TaskMutation::AddAnnotation { entry, description } => {
            let ann = Annotation {
                entry: DateTime::from_timestamp(entry, 0).ok_or_else(|| {
                    FfiError::InvalidInput {
                        message: format!("Invalid epoch: {entry}"),
                    }
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
            clear_xstatus_if_set(task, ops)?;
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
            // can be re-fetched with `get_task`. Auto-clear xstatus.
            clear_xstatus_if_set(task, ops)?;
            task.set_status(Status::Deleted, ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetScheduled { epoch } => {
            let value = epoch.map(|e| e.to_string());
            task.set_value("scheduled", value, ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetStart { epoch } => {
            let value = epoch.map(|e| e.to_string());
            task.set_value("start", value, ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetIsFullDay { value } => {
            let v = if value {
                Some("true".to_string())
            } else {
                None
            };
            task.set_value("is_full_day", v, ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetEstimate { boxes } => {
            if let Some(b) = boxes {
                if b == 0 {
                    return Err(FfiError::InvalidInput {
                        message: "estimate must be > 0".into(),
                    });
                }
            }
            task.set_value("estimate", boxes.map(|b| b.to_string()), ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetRecur { value } => {
            task.set_value("recur", value, ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetMask { value } => {
            task.set_value("mask", value, ops).map_err(FfiError::from)?;
        }
        TaskMutation::SetImask { value } => {
            task.set_value("imask", value.map(|v| v.to_string()), ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetUntil { epoch } => {
            task.set_value("until", epoch.map(|e| e.to_string()), ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetProject { value } => {
            // Always clear project_id: when clearing project (None) this
            // prevents the storage JOIN from resolving a stale name; when
            // setting a name the storage layer overwrites project_id via
            // resolve_project_id, so clearing first is purely defensive.
            task.set_value("project_id", None::<String>, ops)
                .map_err(FfiError::from)?;
            task.set_value("project", value, ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetProjectId { value } => {
            // Validate UUID format if a value is provided.
            if let Some(ref v) = value {
                parse_uuid(v)?;
            }
            task.set_value("project_id", value, ops)
                .map_err(FfiError::from)?;
            // Clear stale project name — the host should set it via SetProject
            // if a human-readable name is needed on the task.
            task.set_value("project", None::<String>, ops)
                .map_err(FfiError::from)?;
        }
        TaskMutation::SetValue { key, value } => {
            // Guard: reject known TaskChampion core keys and dedicated UDA
            // fields — callers should use the typed mutation variant instead.
            let core_keys = [
                "status",
                "description",
                "priority",
                "due",
                "wait",
                "entry",
                "end",
                "modified",
                "parent_id",
                "position",
                "start",
                "project",
                "project_id",
            ];
            if core_keys.contains(&key.as_str())
                || DEDICATED_UDA_FIELDS.contains(&key.as_str())
                || key.starts_with("tag_")
                || key.starts_with("annotation_")
                || key.starts_with("dep_")
            {
                return Err(FfiError::InvalidInput {
                    message: format!(
                        "'{key}' is a known property — use the dedicated mutation variant"
                    ),
                });
            }
            task.set_value(key, value, ops).map_err(FfiError::from)?;
        }
    }
    Ok(())
}
