//! FFI types and exported functions for praxis completion orchestration.

use crate::tree::{ffi_to_task_descendant, FfiTaskDescendant};
use crate::types::FfiError;
use praxis::orchestrate::{plan_completion, CompletionAction, RecurrenceParentInfo};
use praxis::recurrence::mask::parse_mask;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// FFI types
// ---------------------------------------------------------------------------

/// Recurrence parent context when completing a recurring child task.
///
/// Mirrors `praxis::orchestrate::RecurrenceParentInfo`, but uses a raw mask
/// string and `u32` imask for FFI compatibility.
#[derive(uniffi::Record)]
pub struct FfiRecurrenceParentInfo {
    /// UUID of the parent recurring template.
    pub template_uuid: String,
    /// Current raw mask string of the parent template (e.g. `"-+-"`).
    pub current_mask: String,
    /// Index into the parent mask for this child instance.
    pub imask: u32,
}

/// Output of `plan_completion_ffi` — actions the caller should execute.
///
/// Mirrors `praxis::orchestrate::CompletionAction`.
#[derive(uniffi::Enum)]
pub enum FfiCompletionAction {
    /// Complete the task with this UUID.
    CompleteTask { uuid: String },
    /// Update the recurrence template's mask string.
    UpdateRecurrenceMask {
        template_uuid: String,
        new_mask: String,
    },
}

// ---------------------------------------------------------------------------
// Exported functions
// ---------------------------------------------------------------------------

/// Plan all actions needed to complete a task.
///
/// The target task is always the first action. Subsequent actions are:
/// 1. `CompleteTask` for each pending or waiting descendant (tree behavior)
/// 2. `UpdateRecurrenceMask` for the recurrence parent (if applicable)
///
/// Returns `InvalidInput` if any UUID is invalid or if `imask` is out of
/// bounds for the parent mask.
#[uniffi::export]
pub fn plan_completion_ffi(
    target_uuid: String,
    descendants: Vec<FfiTaskDescendant>,
    recurrence_parent: Option<FfiRecurrenceParentInfo>,
) -> Result<Vec<FfiCompletionAction>, FfiError> {
    let target = Uuid::parse_str(&target_uuid).map_err(|e| FfiError::InvalidInput {
        message: format!("invalid target UUID '{target_uuid}': {e}"),
    })?;

    let rust_descs: Result<Vec<_>, _> = descendants
        .into_iter()
        .map(ffi_to_task_descendant)
        .collect();
    let rust_descs = rust_descs?;

    let rust_parent = recurrence_parent
        .map(|p| -> Result<RecurrenceParentInfo, FfiError> {
            let template_uuid =
                Uuid::parse_str(&p.template_uuid).map_err(|e| FfiError::InvalidInput {
                    message: format!("invalid template UUID '{}': {e}", p.template_uuid),
                })?;
            Ok(RecurrenceParentInfo {
                template_uuid,
                current_mask: parse_mask(&p.current_mask),
                imask: p.imask as usize,
            })
        })
        .transpose()?;

    let actions = plan_completion(target, &rust_descs, rust_parent.as_ref()).map_err(|e| {
        FfiError::InvalidInput {
            message: e.to_string(),
        }
    })?;

    Ok(actions
        .into_iter()
        .map(|a| match a {
            CompletionAction::CompleteTask { uuid } => FfiCompletionAction::CompleteTask {
                uuid: uuid.to_string(),
            },
            CompletionAction::UpdateRecurrenceMask {
                template_uuid,
                new_mask,
            } => FfiCompletionAction::UpdateRecurrenceMask {
                template_uuid: template_uuid.to_string(),
                new_mask,
            },
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FfiStatus;

    fn uuid_str() -> String {
        "12345678-1234-1234-1234-123456789abc".to_string()
    }

    fn template_uuid_str() -> String {
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string()
    }

    fn child_uuid_str() -> String {
        "cccccccc-cccc-cccc-cccc-cccccccccccc".to_string()
    }

    fn pending_desc(uuid: String) -> FfiTaskDescendant {
        FfiTaskDescendant {
            uuid,
            status: FfiStatus::Pending,
            has_wait: false,
        }
    }

    fn completed_desc(uuid: String) -> FfiTaskDescendant {
        FfiTaskDescendant {
            uuid,
            status: FfiStatus::Completed,
            has_wait: false,
        }
    }

    // --- Empty descendants, no recurrence → exactly 1 CompleteTask ---

    #[test]
    fn empty_descendants_no_recurrence() {
        let actions = plan_completion_ffi(uuid_str(), vec![], None).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            FfiCompletionAction::CompleteTask { uuid } if *uuid == uuid_str()
        ));
    }

    // --- Task with descendants only → CompleteTask for target + pending descendants ---

    #[test]
    fn descendants_only_no_recurrence() {
        let child1 = child_uuid_str();
        let child2 = "dddddddd-dddd-dddd-dddd-dddddddddddd".to_string();
        let descs = vec![
            pending_desc(child1.clone()),
            completed_desc(child2.clone()), // completed should be skipped
        ];
        let actions = plan_completion_ffi(uuid_str(), descs, None).unwrap();

        let complete_uuids: Vec<&str> = actions
            .iter()
            .filter_map(|a| {
                if let FfiCompletionAction::CompleteTask { uuid } = a {
                    Some(uuid.as_str())
                } else {
                    None
                }
            })
            .collect();

        // target + child1 (pending); child2 (completed) skipped
        assert_eq!(complete_uuids.len(), 2);
        assert!(complete_uuids.contains(&uuid_str().as_str()));
        assert!(complete_uuids.contains(&child1.as_str()));
        assert!(!complete_uuids.contains(&child2.as_str()));
    }

    // --- Task with recurrence parent only → CompleteTask + UpdateRecurrenceMask ---

    #[test]
    fn recurrence_parent_only_no_descendants() {
        let parent = FfiRecurrenceParentInfo {
            template_uuid: template_uuid_str(),
            current_mask: "-+-".to_string(),
            imask: 0,
        };
        let actions = plan_completion_ffi(uuid_str(), vec![], Some(parent)).unwrap();
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            FfiCompletionAction::CompleteTask { uuid } if *uuid == uuid_str()
        ));
        assert!(matches!(
            &actions[1],
            FfiCompletionAction::UpdateRecurrenceMask { template_uuid, new_mask }
            if *template_uuid == template_uuid_str() && new_mask == "++-"
        ));
    }

    // --- Task with both descendants and recurrence parent ---

    #[test]
    fn descendants_and_recurrence_parent() {
        let child = child_uuid_str();
        let parent = FfiRecurrenceParentInfo {
            template_uuid: template_uuid_str(),
            current_mask: "--".to_string(),
            imask: 1,
        };
        let actions =
            plan_completion_ffi(uuid_str(), vec![pending_desc(child)], Some(parent)).unwrap();
        assert_eq!(actions.len(), 3);
        // Last action should be the mask update
        assert!(matches!(
            &actions[2],
            FfiCompletionAction::UpdateRecurrenceMask { .. }
        ));
    }

    // --- Invalid UUID ---

    #[test]
    fn invalid_target_uuid() {
        let result = plan_completion_ffi("not-a-uuid".to_string(), vec![], None);
        assert!(matches!(result, Err(FfiError::InvalidInput { .. })));
    }

    #[test]
    fn invalid_template_uuid() {
        let parent = FfiRecurrenceParentInfo {
            template_uuid: "bad-uuid".to_string(),
            current_mask: "-".to_string(),
            imask: 0,
        };
        let result = plan_completion_ffi(uuid_str(), vec![], Some(parent));
        assert!(matches!(result, Err(FfiError::InvalidInput { .. })));
    }

    // --- Out-of-bounds imask ---

    #[test]
    fn out_of_bounds_imask() {
        let parent = FfiRecurrenceParentInfo {
            template_uuid: template_uuid_str(),
            current_mask: "-+".to_string(),
            imask: 99, // way out of bounds
        };
        let result = plan_completion_ffi(uuid_str(), vec![], Some(parent));
        assert!(matches!(result, Err(FfiError::InvalidInput { .. })));
    }
}
