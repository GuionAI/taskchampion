use uuid::Uuid;

use crate::errors::RecurrenceError;
use crate::recurrence::mask::RecurrenceMask;
use crate::recurrence::orchestrate::{update_mask_for_child, ChildStatusChange};
use crate::tree::behavior::{descendants_to_complete, TaskDescendant};
use taskchampion::Status;

/// Info about the recurrence parent when completing a recurring child.
pub struct RecurrenceParentInfo {
    pub template_uuid: Uuid,
    /// Parsed mask of the parent template. Using `RecurrenceMask` (rather than a raw
    /// string) pushes parse errors to the call site and avoids redundant re-parsing.
    pub current_mask: RecurrenceMask,
    pub imask: usize,
}

/// Actions for completing a task that may have descendants and/or be a recurring child.
#[derive(Debug)]
pub enum CompletionAction {
    CompleteTask {
        uuid: Uuid,
    },
    UpdateRecurrenceMask {
        template_uuid: Uuid,
        new_mask: String,
    },
}

/// Given a task being completed, return all actions needed.
///
/// The target task is always the first action. Subsequent actions are:
/// 1. `CompleteTask` for each pending or waiting descendant (tree behavior)
/// 2. `UpdateRecurrenceMask` for the recurrence parent (if applicable)
///
/// # Errors
///
/// Returns `Err` if `recurrence_parent` is provided and `update_mask_for_child`
/// fails (e.g., `imask` is out of bounds for the parent mask).
pub fn plan_completion(
    target_uuid: Uuid,
    descendants: &[TaskDescendant],
    recurrence_parent: Option<&RecurrenceParentInfo>,
) -> Result<Vec<CompletionAction>, RecurrenceError> {
    let mut actions = Vec::new();

    // Always include the target task itself first
    actions.push(CompletionAction::CompleteTask { uuid: target_uuid });

    // Tree behavior: complete pending descendants
    for uuid in descendants_to_complete(descendants) {
        actions.push(CompletionAction::CompleteTask { uuid });
    }

    // Recurrence behavior: update parent mask
    if let Some(parent) = recurrence_parent {
        let change = ChildStatusChange {
            template_uuid: parent.template_uuid,
            imask: parent.imask,
            new_status: Status::Completed,
            has_wait: false,
        };
        let new_mask = update_mask_for_child(&parent.current_mask, &change)?;
        actions.push(CompletionAction::UpdateRecurrenceMask {
            template_uuid: parent.template_uuid,
            new_mask,
        });
    }

    Ok(actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recurrence::mask::parse_mask;
    use crate::tree::behavior::TaskDescendant;
    use pretty_assertions::assert_eq;
    use taskchampion::Status;
    use uuid::Uuid;

    fn uid() -> Uuid {
        Uuid::new_v4()
    }

    fn desc(uuid: Uuid, status: Status) -> TaskDescendant {
        TaskDescendant {
            uuid,
            status,
            has_wait: false,
        }
    }

    fn desc_waiting(uuid: Uuid) -> TaskDescendant {
        TaskDescendant {
            uuid,
            status: Status::Pending,
            has_wait: true,
        }
    }

    fn parent_info(template_uuid: Uuid, mask: &str, imask: usize) -> RecurrenceParentInfo {
        RecurrenceParentInfo {
            template_uuid,
            current_mask: parse_mask(mask),
            imask,
        }
    }

    #[test]
    fn target_always_first() {
        let target = uid();
        let child = uid();
        let actions = plan_completion(target, &[desc(child, Status::Pending)], None).unwrap();
        assert!(matches!(&actions[0], CompletionAction::CompleteTask { uuid } if *uuid == target));
    }

    #[test]
    fn descendants_no_recurrence() {
        let target = uid();
        let d1 = uid();
        let d2 = uid();
        let descendants = vec![desc(d1, Status::Pending), desc(d2, Status::Completed)];
        let actions = plan_completion(target, &descendants, None).unwrap();

        let complete_uuids: Vec<Uuid> = actions
            .iter()
            .filter_map(|a| {
                if let CompletionAction::CompleteTask { uuid } = a {
                    Some(*uuid)
                } else {
                    None
                }
            })
            .collect();

        // target + d1 (pending); d2 (completed) skipped
        assert_eq!(complete_uuids.len(), 2);
        assert!(complete_uuids.contains(&target));
        assert!(complete_uuids.contains(&d1));
        assert!(!complete_uuids.contains(&d2));
    }

    #[test]
    fn recurrence_no_descendants() {
        let target = uid();
        let template_id = uid();
        let parent = parent_info(template_id, "-+-", 0);
        let actions = plan_completion(target, &[], Some(&parent)).unwrap();

        assert_eq!(actions.len(), 2);
        assert!(matches!(&actions[0], CompletionAction::CompleteTask { uuid } if *uuid == target));
        if let CompletionAction::UpdateRecurrenceMask {
            template_uuid,
            new_mask,
        } = &actions[1]
        {
            assert_eq!(*template_uuid, template_id);
            assert_eq!(new_mask, "++-"); // index 0: pending→completed
        } else {
            panic!("expected UpdateRecurrenceMask");
        }
    }

    #[test]
    fn descendants_and_recurrence() {
        let target = uid();
        let child = uid();
        let template_id = uid();
        let parent = parent_info(template_id, "--", 1);
        let actions =
            plan_completion(target, &[desc(child, Status::Pending)], Some(&parent)).unwrap();

        assert_eq!(actions.len(), 3);
        // target, child, mask update
        assert!(matches!(
            &actions[2],
            CompletionAction::UpdateRecurrenceMask { .. }
        ));
    }

    #[test]
    fn empty_descendants_no_recurrence() {
        let target = uid();
        let actions = plan_completion(target, &[], None).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], CompletionAction::CompleteTask { uuid } if *uuid == target));
    }

    #[test]
    fn recurrence_imask_out_of_bounds_returns_error() {
        let target = uid();
        let template_id = uid();
        let parent = parent_info(template_id, "-+", 5); // imask=5 out of bounds
        let result = plan_completion(target, &[], Some(&parent));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RecurrenceError::MaskIndexOutOfBounds { .. }
        ));
    }

    // T1: waiting descendants are included in completion output
    #[test]
    fn waiting_descendants_are_completed() {
        let target = uid();
        let waiting = uid();
        let completed = uid();
        let descendants = vec![
            desc_waiting(waiting),              // waiting → should be completed
            desc(completed, Status::Completed), // already done → skipped
        ];
        let actions = plan_completion(target, &descendants, None).unwrap();

        let complete_uuids: Vec<Uuid> = actions
            .iter()
            .filter_map(|a| {
                if let CompletionAction::CompleteTask { uuid } = a {
                    Some(*uuid)
                } else {
                    None
                }
            })
            .collect();

        assert!(complete_uuids.contains(&target));
        assert!(
            complete_uuids.contains(&waiting),
            "waiting descendant should be completed"
        );
        assert!(!complete_uuids.contains(&completed));
    }
}
