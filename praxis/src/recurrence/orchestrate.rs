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
