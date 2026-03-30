use chrono::{DateTime, Utc};
use taskchampion::Status;

/// A single slot in a recurrence mask, representing the status of one instance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MaskChar {
    /// Task is pending (not yet complete). Represented as '-'.
    Pending,
    /// Task is waiting (pending with a future wait date). Represented as 'W'.
    Waiting,
    /// Task is completed. Represented as '+'.
    Completed,
    /// Task is deleted. Represented as 'X'.
    Deleted,
    /// Status unknown or unrecognized. Represented as '?'.
    ///
    /// Used for `Status::Recurring` and `Status::Unknown(_)` to prevent phantom
    /// instance generation from templates or corrupt data. Unknown slots do NOT
    /// trigger generation — they are treated as "something happened here, leave
    /// it alone."
    Unknown,
}

impl MaskChar {
    fn from_char(c: char) -> MaskChar {
        match c {
            '-' => MaskChar::Pending,
            'W' => MaskChar::Waiting,
            '+' => MaskChar::Completed,
            'X' => MaskChar::Deleted,
            _ => MaskChar::Unknown,
        }
    }

    fn to_char(self) -> char {
        match self {
            MaskChar::Pending => '-',
            MaskChar::Waiting => 'W',
            MaskChar::Completed => '+',
            MaskChar::Deleted => 'X',
            MaskChar::Unknown => '?',
        }
    }
}

/// A structured recurrence mask — a sequence of `MaskChar` slots, one per due date.
///
/// The inner `Vec` is private to prevent arbitrary mutation (e.g. positional
/// inserts or truncation) that would break mask/due-date alignment.
#[derive(Debug, Clone, PartialEq)]
pub struct RecurrenceMask(Vec<MaskChar>);

impl RecurrenceMask {
    /// Construct a mask from a vec of `MaskChar`s.
    pub fn new(chars: Vec<MaskChar>) -> Self {
        RecurrenceMask(chars)
    }

    /// Number of slots in the mask.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True if the mask has no slots.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Get the mask char at position `i`, or `None` if out of bounds.
    pub fn get(&self, i: usize) -> Option<MaskChar> {
        self.0.get(i).copied()
    }

    /// Append a slot to the mask.
    pub fn push(&mut self, c: MaskChar) {
        self.0.push(c);
    }

    /// Iterate over all mask chars.
    pub fn iter(&self) -> impl Iterator<Item = MaskChar> + '_ {
        self.0.iter().copied()
    }
}

/// Parse a mask string (e.g. `"-+XW?"`) into a `RecurrenceMask`.
pub fn parse_mask(mask: &str) -> RecurrenceMask {
    RecurrenceMask(mask.chars().map(MaskChar::from_char).collect())
}

/// Serialize a `RecurrenceMask` back to its string form.
pub fn serialize_mask(mask: &RecurrenceMask) -> String {
    mask.0.iter().map(|c| c.to_char()).collect()
}

/// Return the indices in `[0..due_count)` that need an instance created.
///
/// An index needs generation only if the mask has **no slot** for it
/// (`index >= mask.len()`). Existing slots — including `Unknown` (`'?'`) —
/// are treated as "already accounted for" and do NOT trigger generation.
/// This prevents phantom instance creation from templates or corrupt data.
pub fn ungenerated_indices(mask: &RecurrenceMask, due_count: usize) -> Vec<usize> {
    (0..due_count).filter(|&i| mask.get(i).is_none()).collect()
}

/// Map a task status to its mask character.
///
/// `has_wait` must be true when the task has a future `wait` date (i.e. the task
/// is logically "waiting"). This is required because `tch::Status` has no Waiting
/// variant — waiting is represented as `Status::Pending` + future wait date.
///
/// `Status::Recurring` and `Status::Unknown(_)` both map to `MaskChar::Unknown`
/// (`'?'`). This prevents phantom instance generation from corrupt or unexpected
/// data.
pub fn mask_char_for_status(status: &Status, has_wait: bool) -> MaskChar {
    match status {
        Status::Pending => {
            if has_wait {
                MaskChar::Waiting
            } else {
                MaskChar::Pending
            }
        }
        Status::Completed => MaskChar::Completed,
        Status::Deleted => MaskChar::Deleted,
        Status::Recurring | Status::Unknown(_) => MaskChar::Unknown,
    }
}

/// Check whether the recurrence template has fully expired.
///
/// A template is expired when:
/// - The mask covers all `due_count` slots (no ungenerated instances), AND
/// - No slot is `Pending` (i.e. no outstanding work), AND
/// - `until_reached` is true
///
/// Special cases:
/// - All `Waiting` slots + `until_reached` → expired (no pending work remaining)
/// - `Unknown` (`'?'`) entries are treated as non-pending (forward-compat)
/// - Mask shorter than `due_count` → NOT expired (ungenerated slots remain)
pub fn is_template_expired(mask: &RecurrenceMask, due_count: usize, until_reached: bool) -> bool {
    if !until_reached {
        return false;
    }
    // Mask must cover all slots
    if mask.len() < due_count {
        return false;
    }
    // No Pending slots allowed
    !mask.0.contains(&MaskChar::Pending)
}

/// High-level diff: given a mask and due dates, return `(index, date)` pairs for
/// instances that still need to be created.
///
/// An instance needs creation only if its slot is missing from the mask.
pub fn recurrence_diff(
    mask: &RecurrenceMask,
    due_dates: &[DateTime<Utc>],
) -> Vec<(usize, DateTime<Utc>)> {
    ungenerated_indices(mask, due_dates.len())
        .into_iter()
        .filter_map(|i| due_dates.get(i).map(|&d| (i, d)))
        .collect()
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
    fn parse_serialize_round_trip() {
        let input = "-+XW?";
        let mask = parse_mask(input);
        assert_eq!(
            mask.iter().collect::<Vec<_>>(),
            vec![
                MaskChar::Pending,
                MaskChar::Completed,
                MaskChar::Deleted,
                MaskChar::Waiting,
                MaskChar::Unknown,
            ]
        );
        assert_eq!(serialize_mask(&mask), input);
    }

    #[test]
    fn parse_empty_mask() {
        let mask = parse_mask("");
        assert!(mask.is_empty());
        assert_eq!(serialize_mask(&mask), "");
    }

    #[test]
    fn ungenerated_partial_mask() {
        // mask "-+" covers indices 0,1; indices 2,3 need generation
        let mask = parse_mask("-+");
        let indices = ungenerated_indices(&mask, 4);
        assert_eq!(indices, vec![2, 3]);
    }

    #[test]
    fn ungenerated_empty_mask() {
        let mask = parse_mask("");
        let indices = ungenerated_indices(&mask, 3);
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn ungenerated_full_mask() {
        let mask = parse_mask("-+X");
        let indices = ungenerated_indices(&mask, 3);
        assert_eq!(indices, vec![] as Vec<usize>);
    }

    #[test]
    fn ungenerated_ignores_unknown_slots() {
        // '?' entries are NOT missing — they do NOT trigger instance generation.
        // This is the core design invariant: Unknown prevents phantom generation.
        let mask = parse_mask("?");
        let indices = ungenerated_indices(&mask, 1);
        assert_eq!(indices, vec![] as Vec<usize>);
    }

    #[test]
    fn mask_char_for_pending() {
        assert_eq!(
            mask_char_for_status(&Status::Pending, false),
            MaskChar::Pending
        );
    }

    #[test]
    fn mask_char_for_waiting() {
        assert_eq!(
            mask_char_for_status(&Status::Pending, true),
            MaskChar::Waiting
        );
    }

    #[test]
    fn mask_char_for_completed() {
        assert_eq!(
            mask_char_for_status(&Status::Completed, false),
            MaskChar::Completed
        );
    }

    #[test]
    fn mask_char_for_deleted() {
        assert_eq!(
            mask_char_for_status(&Status::Deleted, false),
            MaskChar::Deleted
        );
    }

    #[test]
    fn mask_char_for_recurring() {
        assert_eq!(
            mask_char_for_status(&Status::Recurring, false),
            MaskChar::Unknown
        );
    }

    #[test]
    fn mask_char_for_unknown_status() {
        assert_eq!(
            mask_char_for_status(&Status::Unknown("custom".into()), false),
            MaskChar::Unknown
        );
    }

    #[test]
    fn template_expired_all_completed() {
        let mask = parse_mask("++X");
        assert!(is_template_expired(&mask, 3, true));
    }

    #[test]
    fn template_expired_all_waiting() {
        let mask = parse_mask("WW");
        assert!(is_template_expired(&mask, 2, true));
    }

    #[test]
    fn template_not_expired_has_pending() {
        let mask = parse_mask("+-");
        assert!(!is_template_expired(&mask, 2, true));
    }

    #[test]
    fn template_not_expired_until_not_reached() {
        let mask = parse_mask("++");
        assert!(!is_template_expired(&mask, 2, false));
    }

    #[test]
    fn template_not_expired_mask_shorter() {
        // mask has 2 slots but 3 due dates — ungenerated slot remains
        let mask = parse_mask("++");
        assert!(!is_template_expired(&mask, 3, true));
    }

    #[test]
    fn template_expired_unknown_entries() {
        // '?' entries don't block expiry
        let mask = parse_mask("+?X");
        assert!(is_template_expired(&mask, 3, true));
    }

    #[test]
    fn recurrence_diff_basic() {
        let mask = parse_mask("-+");
        let dates = vec![dt(2024, 1, 1), dt(2024, 2, 1), dt(2024, 3, 1)];
        let diff = recurrence_diff(&mask, &dates);
        assert_eq!(diff, vec![(2, dt(2024, 3, 1))]);
    }

    #[test]
    fn recurrence_diff_empty_mask() {
        let mask = parse_mask("");
        let dates = vec![dt(2024, 1, 1), dt(2024, 2, 1)];
        let diff = recurrence_diff(&mask, &dates);
        assert_eq!(diff, vec![(0, dt(2024, 1, 1)), (1, dt(2024, 2, 1))]);
    }

    #[test]
    fn recurrence_diff_unknown_not_generated() {
        // '?' slot must NOT trigger creation — Unknown prevents phantom generation.
        let mask = parse_mask("?+");
        let dates = vec![dt(2024, 1, 1), dt(2024, 2, 1)];
        let diff = recurrence_diff(&mask, &dates);
        assert_eq!(diff, vec![] as Vec<(usize, DateTime<Utc>)>);
    }
}
