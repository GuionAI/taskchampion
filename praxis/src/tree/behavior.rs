use taskchampion::Status;
use uuid::Uuid;

/// A task in a tree hierarchy, with its status and wait state.
///
/// `has_wait` must be true when the task has a future `wait` date. This is
/// required because `tch::Status` has no Waiting variant — waiting is
/// represented as `Status::Pending` + future wait date.
pub struct TaskDescendant {
    pub uuid: Uuid,
    pub status: Status,
    /// True when the task has a future `wait` date (logically "waiting").
    pub has_wait: bool,
}

/// Given a list of descendants, return the UUIDs that should be auto-completed.
///
/// Rule (mirrors tw's CmdDone behavior): all Pending and Waiting descendants are
/// completed. Completed, Deleted, Recurring, and Unknown are skipped.
///
/// Note: both `Status::Pending` with `has_wait=false` (pending) and
/// `Status::Pending` with `has_wait=true` (waiting) are completed — tw
/// auto-completes both. The `has_wait` field does not affect this decision;
/// it is included in `TaskDescendant` for API symmetry with callers that
/// build descriptors from full task data.
pub fn descendants_to_complete(descendants: &[TaskDescendant]) -> Vec<Uuid> {
    descendants
        .iter()
        .filter_map(|d| {
            let should_complete = matches!(d.status, Status::Pending);
            if should_complete {
                Some(d.uuid)
            } else {
                None
            }
        })
        .collect()
}

/// Given a list of descendants, return `(pending_count, all_uuids)`.
///
/// - `pending_count`: number of Pending + Waiting descendants (both `Status::Pending`
///   regardless of `has_wait`). The caller uses this to decide whether to prompt
///   the user before deletion.
/// - `all_uuids`: UUIDs of *all* descendants regardless of status (to be deleted).
///
/// Note: `Unknown(_)` and `Recurring` descendants are included in `all_uuids`
/// but do NOT count toward `pending_count`. `has_wait` does not affect the
/// pending count — both pending and waiting are counted together.
pub fn descendants_to_delete(descendants: &[TaskDescendant]) -> (usize, Vec<Uuid>) {
    let mut pending_count = 0usize;
    let mut all_uuids = Vec::with_capacity(descendants.len());

    for d in descendants {
        all_uuids.push(d.uuid);
        if matches!(d.status, Status::Pending) {
            pending_count += 1;
        }
    }

    (pending_count, all_uuids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

    fn desc(uuid: Uuid, status: Status, has_wait: bool) -> TaskDescendant {
        TaskDescendant {
            uuid,
            status,
            has_wait,
        }
    }

    fn uid() -> Uuid {
        Uuid::new_v4()
    }

    #[test]
    fn complete_filters_pending_and_waiting() {
        let u1 = uid();
        let u2 = uid();
        let u3 = uid();
        let u4 = uid();

        let descendants = vec![
            desc(u1, Status::Pending, false),   // pending → complete
            desc(u2, Status::Pending, true),    // waiting → complete
            desc(u3, Status::Completed, false), // skip
            desc(u4, Status::Deleted, false),   // skip
        ];
        let result = descendants_to_complete(&descendants);
        assert!(result.contains(&u1));
        assert!(result.contains(&u2));
        assert!(!result.contains(&u3));
        assert!(!result.contains(&u4));
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn complete_empty_input() {
        let result = descendants_to_complete(&[]);
        assert_eq!(result, vec![] as Vec<Uuid>);
    }

    #[test]
    fn complete_all_completed() {
        let u1 = uid();
        let u2 = uid();
        let descendants = vec![
            desc(u1, Status::Completed, false),
            desc(u2, Status::Completed, false),
        ];
        assert_eq!(descendants_to_complete(&descendants), vec![] as Vec<Uuid>);
    }

    #[test]
    fn complete_skips_recurring_and_unknown() {
        let u1 = uid();
        let u2 = uid();
        let descendants = vec![
            desc(u1, Status::Recurring, false),
            desc(u2, Status::Unknown("custom".into()), false),
        ];
        assert_eq!(descendants_to_complete(&descendants), vec![] as Vec<Uuid>);
    }

    #[test]
    fn delete_counts_pending_and_waiting() {
        let u1 = uid();
        let u2 = uid();
        let u3 = uid();
        let u4 = uid();

        let descendants = vec![
            desc(u1, Status::Pending, false),   // pending
            desc(u2, Status::Pending, true),    // waiting
            desc(u3, Status::Completed, false), // not pending
            desc(u4, Status::Deleted, false),   // not pending
        ];
        let (pending_count, all_uuids) = descendants_to_delete(&descendants);
        assert_eq!(pending_count, 2);
        assert_eq!(all_uuids.len(), 4);
        assert!(all_uuids.contains(&u1));
        assert!(all_uuids.contains(&u2));
        assert!(all_uuids.contains(&u3));
        assert!(all_uuids.contains(&u4));
    }

    #[test]
    fn delete_no_pending_descendants() {
        let u1 = uid();
        let u2 = uid();
        let descendants = vec![
            desc(u1, Status::Completed, false),
            desc(u2, Status::Deleted, false),
        ];
        let (pending_count, all_uuids) = descendants_to_delete(&descendants);
        assert_eq!(pending_count, 0);
        assert_eq!(all_uuids.len(), 2);
    }

    #[test]
    fn delete_unknown_not_counted_as_pending() {
        let u1 = uid();
        let u2 = uid();
        let descendants = vec![
            desc(u1, Status::Unknown("x".into()), false),
            desc(u2, Status::Recurring, false),
        ];
        let (pending_count, all_uuids) = descendants_to_delete(&descendants);
        assert_eq!(pending_count, 0);
        assert_eq!(all_uuids.len(), 2);
    }
}
