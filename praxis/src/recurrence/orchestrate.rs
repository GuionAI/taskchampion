use chrono::{DateTime, Utc};
use std::collections::HashMap;
use taskchampion::Status;
use uuid::Uuid;

use crate::errors::RecurrenceError;
use crate::recurrence::mask::{mask_char_for_status, parse_mask, serialize_mask};

/// Input: a recurring template's current state.
pub struct RecurringTemplate {
    pub uuid: Uuid,
    pub due: DateTime<Utc>,
    /// Raw recurrence spec string (e.g. "weekly", "P1M"), parsed internally.
    pub recur: String,
    /// Raw mask string (e.g. "-+XW"), parsed internally.
    pub mask: String,
    pub until: Option<DateTime<Utc>>,
    /// If set, children inherit a wait offset relative to their due date.
    pub wait: Option<DateTime<Utc>>,
    /// Fields to clone onto child tasks (description, project, tags, etc.).
    /// Praxis treats these as an opaque blob — the caller decides what to include.
    pub cloneable_fields: HashMap<String, String>,
}

/// Output: actions for the caller to execute against storage.
///
/// Actions are ordered per template: creates first, then mask update, then
/// expiration. Across multiple templates, each template's actions appear
/// consecutively in this same order.
pub enum RecurrenceAction {
    /// Create a child task instance for the given template.
    CreateChild {
        template_uuid: Uuid,
        imask: usize,
        due: DateTime<Utc>,
        /// Wait date for the child (if template has wait, offset from due).
        wait: Option<DateTime<Utc>>,
        /// Fields cloned from the template (description, project, tags, etc.).
        ///
        /// Note: for templates with large catch-ups (many missing instances),
        /// this clone is repeated for each child. If cloneable_fields is large,
        /// callers may want to batch storage writes to amortize the cost.
        cloneable_fields: HashMap<String, String>,
    },
    /// Update the template's mask string.
    UpdateTemplateMask {
        template_uuid: Uuid,
        new_mask: String,
    },
    /// Template is fully expired — caller should mark it for deletion.
    ExpireTemplate { template_uuid: Uuid },
}

/// Input for a mask update when a child task's status changes.
pub struct ChildStatusChange {
    pub child_uuid: Uuid,
    pub template_uuid: Uuid,
    pub imask: usize,
    pub new_status: Status,
    /// True when the child has a future `wait` date (logically "waiting").
    pub has_wait: bool,
}

/// Given a template's current mask and a child's status change, return the updated
/// mask string.
///
/// This is the Rust equivalent of TW's `updateRecurrenceMask()`.
///
/// Returns `Err(RecurrenceError::MaskIndexOutOfBounds)` if `change.imask` is out
/// of bounds for the current mask.
pub fn update_mask_for_child(
    current_mask: &str,
    change: &ChildStatusChange,
) -> Result<String, RecurrenceError> {
    let mut mask = parse_mask(current_mask);
    let new_char = mask_char_for_status(&change.new_status, change.has_wait);
    mask.set(change.imask, new_char)?;
    Ok(serialize_mask(&mask))
}

/// Compute a child task's wait date by preserving the template's wait-to-due delta.
///
/// If the template has both `due` and `wait`, the offset is:
///   `offset = template.wait - template.due`
/// The child's wait date is then:
///   `child_wait = child.due + offset`
///
/// The offset may be negative (wait is before due) or positive (wait is after due).
/// Returns `None` if template has no wait, or if date arithmetic overflows.
pub(crate) fn compute_child_wait(
    template_due: DateTime<Utc>,
    template_wait: Option<DateTime<Utc>>,
    child_due: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let wait = template_wait?;
    let offset = wait.signed_duration_since(template_due);
    child_due.checked_add_signed(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    fn dt(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    fn uid() -> Uuid {
        Uuid::new_v4()
    }

    fn change_at(imask: usize, status: Status, has_wait: bool) -> ChildStatusChange {
        ChildStatusChange {
            child_uuid: uid(),
            template_uuid: uid(),
            imask,
            new_status: status,
            has_wait,
        }
    }

    #[test]
    fn update_mask_child_completed() {
        // mask "-+-" = ['-', '+', '-']; set index 0 to Completed → "++-"
        let change = change_at(0, Status::Completed, false);
        let result = update_mask_for_child("-+-", &change).unwrap();
        assert_eq!(result, "++-");
    }

    #[test]
    fn update_mask_child_deleted() {
        let change = change_at(0, Status::Deleted, false);
        let result = update_mask_for_child("-+W", &change).unwrap();
        assert_eq!(result, "X+W");
    }

    #[test]
    fn update_mask_child_pending_no_wait() {
        let change = change_at(1, Status::Pending, false);
        let result = update_mask_for_child("++", &change).unwrap();
        assert_eq!(result, "+-");
    }

    #[test]
    fn update_mask_child_pending_has_wait() {
        let change = change_at(0, Status::Pending, true);
        let result = update_mask_for_child("-+", &change).unwrap();
        assert_eq!(result, "W+");
    }

    #[test]
    fn update_mask_imask_out_of_bounds() {
        let change = change_at(5, Status::Completed, false);
        let err = update_mask_for_child("-+", &change).unwrap_err();
        assert!(matches!(
            err,
            RecurrenceError::MaskIndexOutOfBounds { index: 5, len: 2 }
        ));
    }

    #[test]
    fn update_mask_round_trip_deleted_then_pending() {
        // Start with '-+', mark index 0 deleted, then back to pending
        let change_delete = change_at(0, Status::Deleted, false);
        let after_delete = update_mask_for_child("-+", &change_delete).unwrap();
        assert_eq!(after_delete, "X+");

        let change_pending = change_at(0, Status::Pending, false);
        let after_pending = update_mask_for_child(&after_delete, &change_pending).unwrap();
        assert_eq!(after_pending, "-+");
    }

    #[test]
    fn update_mask_preserves_other_slots() {
        let change = change_at(2, Status::Completed, false);
        let result = update_mask_for_child("-W-", &change).unwrap();
        assert_eq!(result, "-W+");
    }

    #[test]
    fn compute_child_wait_negative_offset() {
        // template due=Jan1, wait=Dec25 (7 days before due)
        // child due=Feb1 → child wait=Jan25 (7 days before Feb1)
        let result = compute_child_wait(
            dt(2024, 1, 1),
            Some(dt(2023, 12, 25)),
            dt(2024, 2, 1),
        );
        assert_eq!(result, Some(dt(2024, 1, 25)));
    }

    #[test]
    fn compute_child_wait_positive_offset() {
        // template due=Jan1, wait=Jan8 (7 days after due)
        // child due=Feb1 → child wait=Feb8
        let result = compute_child_wait(
            dt(2024, 1, 1),
            Some(dt(2024, 1, 8)),
            dt(2024, 2, 1),
        );
        assert_eq!(result, Some(dt(2024, 2, 8)));
    }

    #[test]
    fn compute_child_wait_no_template_wait() {
        let result = compute_child_wait(dt(2024, 1, 1), None, dt(2024, 2, 1));
        assert_eq!(result, None);
    }
}
