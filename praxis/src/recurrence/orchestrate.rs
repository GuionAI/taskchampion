use chrono::{DateTime, Utc};
use std::collections::HashMap;
use taskchampion::Status;
use uuid::Uuid;

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

    fn dt(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
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
