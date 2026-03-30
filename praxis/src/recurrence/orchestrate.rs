use chrono::{DateTime, Utc};
use std::collections::HashMap;
use taskchampion::Status;
use uuid::Uuid;

use crate::errors::RecurrenceError;
use crate::recurrence::generate::generate_due_dates;
use crate::recurrence::mask::{
    is_template_expired, mask_char_for_status, parse_mask, recurrence_diff, serialize_mask,
    MaskChar,
};
use crate::recurrence::spec::parse_spec;

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

/// Core recurrence orchestration: given templates and the current time, return all
/// actions needed to bring task storage up to date.
///
/// This is the Rust equivalent of TW's `handleRecurrence()` + `handleUntil()`.
/// It is a pure function — no storage access. Callers provide templates and
/// execute the returned actions against storage.
///
/// Action ordering is per-template: for each template, creates appear first,
/// followed by the mask update (if any), followed by expiration (if applicable).
/// Actions from different templates appear consecutively in template input order.
///
/// `future_limit` matches TW's `recurrence.limit` config (default 1): it limits
/// how many future (not-yet-due) instances are pre-generated per template.
///
/// # Errors
///
/// Returns `Err` if any template's `recur` field cannot be parsed.
pub fn reconcile(
    templates: &[RecurringTemplate],
    now: DateTime<Utc>,
    future_limit: usize,
) -> Result<Vec<RecurrenceAction>, RecurrenceError> {
    let mut actions = Vec::new();

    for template in templates {
        let spec = parse_spec(&template.recur)?;
        let gen = generate_due_dates(&spec, template.due, now, template.until, future_limit);
        // TODO: surface gen.hit_limit as a warning to callers — hitting the safety cap
        // may indicate corrupt or extreme recurrence data.
        let mut mask = parse_mask(&template.mask);
        let missing = recurrence_diff(&mask, &gen.dates);

        // Emit CreateChild for each missing instance
        for (index, due) in &missing {
            let child_wait = compute_child_wait(template.due, template.wait, *due);
            actions.push(RecurrenceAction::CreateChild {
                template_uuid: template.uuid,
                imask: *index,
                due: *due,
                wait: child_wait,
                // Note: cloneable_fields is cloned per child. For templates with many
                // missing instances (large catch-ups), callers may want to batch storage
                // writes to amortize the per-clone cost if cloneable_fields is large.
                cloneable_fields: template.cloneable_fields.clone(),
            });
            // New mask slot: Waiting if child has a future wait date, otherwise Pending
            let mask_char = match child_wait {
                Some(w) if w > now => MaskChar::Waiting,
                _ => MaskChar::Pending,
            };
            mask.push(mask_char);
        }

        // Emit mask update if mask changed
        let new_mask = serialize_mask(&mask);
        if new_mask != template.mask {
            actions.push(RecurrenceAction::UpdateTemplateMask {
                template_uuid: template.uuid,
                new_mask,
            });
        }

        // Check expiry
        if is_template_expired(&mask, gen.dates.len(), gen.until_reached) {
            actions.push(RecurrenceAction::ExpireTemplate {
                template_uuid: template.uuid,
            });
        }
    }

    Ok(actions)
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
        let result = compute_child_wait(dt(2024, 1, 1), Some(dt(2023, 12, 25)), dt(2024, 2, 1));
        assert_eq!(result, Some(dt(2024, 1, 25)));
    }

    #[test]
    fn compute_child_wait_positive_offset() {
        // template due=Jan1, wait=Jan8 (7 days after due)
        // child due=Feb1 → child wait=Feb8
        let result = compute_child_wait(dt(2024, 1, 1), Some(dt(2024, 1, 8)), dt(2024, 2, 1));
        assert_eq!(result, Some(dt(2024, 2, 8)));
    }

    #[test]
    fn compute_child_wait_no_template_wait() {
        let result = compute_child_wait(dt(2024, 1, 1), None, dt(2024, 2, 1));
        assert_eq!(result, None);
    }

    // ── reconcile() tests ───────────────────────────────────────────────────

    fn template(
        uuid: Uuid,
        due: DateTime<Utc>,
        recur: &str,
        mask: &str,
        until: Option<DateTime<Utc>>,
        wait: Option<DateTime<Utc>>,
    ) -> RecurringTemplate {
        RecurringTemplate {
            uuid,
            due,
            recur: recur.to_string(),
            mask: mask.to_string(),
            until,
            wait,
            cloneable_fields: HashMap::new(),
        }
    }

    #[test]
    fn reconcile_empty_templates() {
        let actions = reconcile(&[], dt(2024, 6, 1), 1).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn reconcile_first_run_empty_mask() {
        // Monthly template, due Jan 1, now=Feb 15 → 2 past instances (Jan, Feb)
        // + 1 future (March) with future_limit=1, empty mask → 3 CreateChild + 1 UpdateTemplateMask
        let id = uid();
        let t = template(id, dt(2024, 1, 1), "monthly", "", None, None);
        let now = dt(2024, 2, 15);
        let actions = reconcile(&[t], now, 1).unwrap();

        let creates: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, RecurrenceAction::CreateChild { .. }))
            .collect();
        let mask_updates: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, RecurrenceAction::UpdateTemplateMask { .. }))
            .collect();

        // Jan 1, Feb 1, Mar 1 (1 future)
        assert_eq!(creates.len(), 3);
        assert_eq!(mask_updates.len(), 1);

        // Verify template uuid on mask update
        if let RecurrenceAction::UpdateTemplateMask {
            template_uuid,
            new_mask,
        } = &mask_updates[0]
        {
            assert_eq!(*template_uuid, id);
            assert_eq!(new_mask, "---"); // all pending (wait=None)
        }
    }

    #[test]
    fn reconcile_partial_mask_creates_only_missing() {
        // Monthly template, due Jan 1, mask="-+" means Jan created+pending, Feb created+completed
        // now=Mar 15, future_limit=1 → expect March instance created (index 2)
        let id = uid();
        let t = template(id, dt(2024, 1, 1), "monthly", "-+", None, None);
        let now = dt(2024, 3, 15);
        let actions = reconcile(&[t], now, 1).unwrap();

        let creates: Vec<_> = actions
            .iter()
            .filter_map(|a| {
                if let RecurrenceAction::CreateChild { imask, .. } = a {
                    Some(*imask)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(creates, vec![2, 3]); // Mar (index 2) and Apr (1 future, index 3)
    }

    #[test]
    fn reconcile_full_completed_mask_emits_expire() {
        // Template past until, all completed → ExpireTemplate
        // mask="+++" already covers all 3 generated dates (Jan, Feb, Mar) before until=Mar1
        let id = uid();
        let until = dt(2024, 3, 1);
        let t = template(id, dt(2024, 1, 1), "monthly", "+++", Some(until), None);
        let now = dt(2024, 4, 1); // past until
        let actions = reconcile(&[t], now, 1).unwrap();

        let expires: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, RecurrenceAction::ExpireTemplate { .. }))
            .collect();
        assert_eq!(expires.len(), 1);
        if let RecurrenceAction::ExpireTemplate { template_uuid } = expires[0] {
            assert_eq!(*template_uuid, id);
        }
    }

    #[test]
    fn reconcile_pending_children_no_expire() {
        // Template past until but has pending slot → no ExpireTemplate
        let id = uid();
        let until = dt(2024, 2, 1);
        let t = template(id, dt(2024, 1, 1), "monthly", "-+", Some(until), None);
        let now = dt(2024, 3, 1);
        let actions = reconcile(&[t], now, 1).unwrap();

        let expires: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, RecurrenceAction::ExpireTemplate { .. }))
            .collect();
        assert!(expires.is_empty());
    }

    #[test]
    fn reconcile_invalid_spec_returns_error() {
        let id = uid();
        let t = template(id, dt(2024, 1, 1), "not-a-spec", "", None, None);
        let result = reconcile(&[t], dt(2024, 2, 1), 1);
        assert!(result.is_err());
    }

    #[test]
    fn reconcile_multiple_templates_ordered() {
        let id1 = uid();
        let id2 = uid();
        let t1 = template(id1, dt(2024, 1, 1), "monthly", "", None, None);
        let t2 = template(id2, dt(2024, 2, 1), "monthly", "", None, None);
        let now = dt(2024, 3, 1);
        let actions = reconcile(&[t1, t2], now, 1).unwrap();

        // All creates for t1 should appear before creates for t2
        let uuids: Vec<Uuid> = actions
            .iter()
            .filter_map(|a| match a {
                RecurrenceAction::CreateChild { template_uuid, .. } => Some(*template_uuid),
                RecurrenceAction::UpdateTemplateMask { template_uuid, .. } => Some(*template_uuid),
                RecurrenceAction::ExpireTemplate { template_uuid } => Some(*template_uuid),
            })
            .collect();

        let first_t2 = uuids.iter().position(|&u| u == id2).unwrap();
        let last_t1 = uuids.iter().rposition(|&u| u == id1).unwrap();
        assert!(
            last_t1 < first_t2,
            "t1 actions should all precede t2 actions"
        );
    }

    #[test]
    fn reconcile_wait_offset_preserved_in_create() {
        // template due=Jan1, wait=Dec25 (7 days before), now=Feb15, future_limit=1
        // child due=Jan1 → child wait=Dec25 (past now) → MaskChar::Pending
        // child due=Feb1 → child wait=Jan25 (past now) → MaskChar::Pending
        // child due=Mar1 → child wait=Feb23 (past now) → MaskChar::Pending
        let id = uid();
        let t = RecurringTemplate {
            uuid: id,
            due: dt(2024, 1, 1),
            recur: "monthly".to_string(),
            mask: "".to_string(),
            until: None,
            wait: Some(dt(2023, 12, 25)),
            cloneable_fields: HashMap::new(),
        };
        let now = dt(2024, 2, 15);
        let actions = reconcile(&[t], now, 1).unwrap();

        let create_waits: Vec<Option<DateTime<Utc>>> = actions
            .iter()
            .filter_map(|a| {
                if let RecurrenceAction::CreateChild { wait, .. } = a {
                    Some(*wait)
                } else {
                    None
                }
            })
            .collect();

        // All 3 children should have wait dates (7 days before their due)
        assert_eq!(create_waits.len(), 3);
        assert_eq!(create_waits[0], Some(dt(2023, 12, 25))); // Jan1 child: Dec25
        assert_eq!(create_waits[1], Some(dt(2024, 1, 25))); // Feb1 child: Jan25
        assert_eq!(create_waits[2], Some(dt(2024, 2, 23))); // Mar1 child: Feb23 (Mar1 - 7 days)
    }

    #[test]
    fn reconcile_new_slot_waiting_when_child_wait_in_future() {
        // template due=Jan1, wait=Jan8 (7 days after due) → wait is AFTER due
        // now=Jan15, future_limit=1
        // child Jan1: wait=Jan8, Jan8 < now=Jan15 → Pending
        // child Feb1: wait=Feb8, Feb8 > now=Jan15 → Waiting
        let id = uid();
        let t = RecurringTemplate {
            uuid: id,
            due: dt(2024, 1, 1),
            recur: "monthly".to_string(),
            mask: "".to_string(),
            until: None,
            wait: Some(dt(2024, 1, 8)),
            cloneable_fields: HashMap::new(),
        };
        let now = dt(2024, 1, 15);
        let actions = reconcile(&[t], now, 1).unwrap();

        if let Some(RecurrenceAction::UpdateTemplateMask { new_mask, .. }) = actions
            .iter()
            .find(|a| matches!(a, RecurrenceAction::UpdateTemplateMask { .. }))
        {
            assert_eq!(new_mask, "-W"); // Jan→Pending (wait past), Feb→Waiting (wait future)
        } else {
            panic!("expected UpdateTemplateMask action");
        }
    }

    #[test]
    fn reconcile_mask_longer_than_dates_no_crash() {
        // mask has more slots than dates → no spurious creates
        let id = uid();
        let t = template(id, dt(2024, 1, 1), "monthly", "---", None, None);
        // now=Feb 5 → only Jan past, future_limit=1 → dates=[Jan1, Feb1]
        let now = dt(2024, 2, 5);
        let actions = reconcile(&[t], now, 1).unwrap();

        // mask already covers all 2 dates → no creates
        let creates: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, RecurrenceAction::CreateChild { .. }))
            .collect();
        assert!(creates.is_empty());
    }

    #[test]
    fn reconcile_future_limit_zero_no_future_creates() {
        // future_limit=0 → only past/current instances
        let id = uid();
        let t = template(id, dt(2024, 1, 1), "monthly", "", None, None);
        // now=Feb15 → past: Jan1, Feb1; no future with limit=0
        let now = dt(2024, 2, 15);
        let actions = reconcile(&[t], now, 0).unwrap();
        let creates: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, RecurrenceAction::CreateChild { .. }))
            .collect();
        assert_eq!(creates.len(), 2);
    }

    #[test]
    fn reconcile_idempotent() {
        // After reconciling and applying creates, reconciling again produces no new creates
        // (Simulate by passing a mask that already covers all generated instances)
        let id = uid();
        // Monthly, due=Jan1, now=Mar15, future_limit=1
        // Dates generated: Jan1, Feb1, Mar1 (past) + Apr1 (1 future) = 4 dates
        // After first reconcile we'd have mask="----" (4 pending slots)
        let t = template(id, dt(2024, 1, 1), "monthly", "----", None, None);
        let now = dt(2024, 3, 15);
        let actions = reconcile(&[t], now, 1).unwrap();

        let creates: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, RecurrenceAction::CreateChild { .. }))
            .collect();
        assert!(
            creates.is_empty(),
            "should be idempotent — no new creates when mask is already up to date"
        );
    }
}
