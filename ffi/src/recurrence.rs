//! FFI types and exported functions for praxis recurrence support.

use crate::replica_ops::parse_uuid_ctx;
use crate::types::{FfiError, FfiStatus};
use praxis::recurrence::mask::{mask_char_for_status, parse_mask, recurrence_diff};
use praxis::recurrence::orchestrate::{ChildStatusChange, RecurrenceAction, RecurringTemplate};
use praxis::recurrence::spec::{parse_spec, RecurrenceSpec};
use std::collections::HashMap;
use taskchampion::Status;

// ---------------------------------------------------------------------------
// FFI types
// ---------------------------------------------------------------------------

/// Recurrence spec — mirrors `praxis::recurrence::spec::RecurrenceSpec`.
///
/// UniFFI requires named fields for all non-unit enum variants.
#[derive(uniffi::Enum)]
pub enum FfiRecurrenceSpec {
    // Named periods (unit variants — no fields needed)
    Daily,
    Weekdays,
    Weekly,
    Biweekly,
    Monthly,
    Bimonthly,
    Quarterly,
    Semiannual,
    Annual,
    Biannual,
    // N-unit shorthand
    NMonths { n: u32 },
    NQuarters { n: u32 },
    NYears { n: u32 },
    NDays { n: u32 },
    NWeeks { n: u32 },
    // ISO 8601 durations
    IsoMonths { n: u32 },
    IsoYears { n: u32 },
    IsoDays { n: u32 },
    IsoWeeks { n: u32 },
    // Fallback: raw duration in seconds
    Seconds { secs: i64 },
}

impl From<RecurrenceSpec> for FfiRecurrenceSpec {
    fn from(s: RecurrenceSpec) -> Self {
        match s {
            RecurrenceSpec::Daily => FfiRecurrenceSpec::Daily,
            RecurrenceSpec::Weekdays => FfiRecurrenceSpec::Weekdays,
            RecurrenceSpec::Weekly => FfiRecurrenceSpec::Weekly,
            RecurrenceSpec::Biweekly => FfiRecurrenceSpec::Biweekly,
            RecurrenceSpec::Monthly => FfiRecurrenceSpec::Monthly,
            RecurrenceSpec::Bimonthly => FfiRecurrenceSpec::Bimonthly,
            RecurrenceSpec::Quarterly => FfiRecurrenceSpec::Quarterly,
            RecurrenceSpec::Semiannual => FfiRecurrenceSpec::Semiannual,
            RecurrenceSpec::Annual => FfiRecurrenceSpec::Annual,
            RecurrenceSpec::Biannual => FfiRecurrenceSpec::Biannual,
            RecurrenceSpec::NMonths(n) => FfiRecurrenceSpec::NMonths { n },
            RecurrenceSpec::NQuarters(n) => FfiRecurrenceSpec::NQuarters { n },
            RecurrenceSpec::NYears(n) => FfiRecurrenceSpec::NYears { n },
            RecurrenceSpec::NDays(n) => FfiRecurrenceSpec::NDays { n },
            RecurrenceSpec::NWeeks(n) => FfiRecurrenceSpec::NWeeks { n },
            RecurrenceSpec::IsoMonths(n) => FfiRecurrenceSpec::IsoMonths { n },
            RecurrenceSpec::IsoYears(n) => FfiRecurrenceSpec::IsoYears { n },
            RecurrenceSpec::IsoDays(n) => FfiRecurrenceSpec::IsoDays { n },
            RecurrenceSpec::IsoWeeks(n) => FfiRecurrenceSpec::IsoWeeks { n },
            RecurrenceSpec::Seconds(secs) => FfiRecurrenceSpec::Seconds { secs },
        }
    }
}

impl From<FfiRecurrenceSpec> for RecurrenceSpec {
    fn from(s: FfiRecurrenceSpec) -> Self {
        match s {
            FfiRecurrenceSpec::Daily => RecurrenceSpec::Daily,
            FfiRecurrenceSpec::Weekdays => RecurrenceSpec::Weekdays,
            FfiRecurrenceSpec::Weekly => RecurrenceSpec::Weekly,
            FfiRecurrenceSpec::Biweekly => RecurrenceSpec::Biweekly,
            FfiRecurrenceSpec::Monthly => RecurrenceSpec::Monthly,
            FfiRecurrenceSpec::Bimonthly => RecurrenceSpec::Bimonthly,
            FfiRecurrenceSpec::Quarterly => RecurrenceSpec::Quarterly,
            FfiRecurrenceSpec::Semiannual => RecurrenceSpec::Semiannual,
            FfiRecurrenceSpec::Annual => RecurrenceSpec::Annual,
            FfiRecurrenceSpec::Biannual => RecurrenceSpec::Biannual,
            FfiRecurrenceSpec::NMonths { n } => RecurrenceSpec::NMonths(n),
            FfiRecurrenceSpec::NQuarters { n } => RecurrenceSpec::NQuarters(n),
            FfiRecurrenceSpec::NYears { n } => RecurrenceSpec::NYears(n),
            FfiRecurrenceSpec::NDays { n } => RecurrenceSpec::NDays(n),
            FfiRecurrenceSpec::NWeeks { n } => RecurrenceSpec::NWeeks(n),
            FfiRecurrenceSpec::IsoMonths { n } => RecurrenceSpec::IsoMonths(n),
            FfiRecurrenceSpec::IsoYears { n } => RecurrenceSpec::IsoYears(n),
            FfiRecurrenceSpec::IsoDays { n } => RecurrenceSpec::IsoDays(n),
            FfiRecurrenceSpec::IsoWeeks { n } => RecurrenceSpec::IsoWeeks(n),
            FfiRecurrenceSpec::Seconds { secs } => RecurrenceSpec::Seconds(secs),
        }
    }
}

/// Result of `generate_due_dates` — the generated dates and metadata.
#[derive(uniffi::Record)]
pub struct FfiGeneratedDates {
    /// Generated due dates as Unix epoch seconds.
    pub dates: Vec<i64>,
    /// True if the `until` boundary was reached during generation.
    pub until_reached: bool,
    /// True if generation stopped at the safety cap (data issue indicator).
    pub hit_limit: bool,
}

/// A single mask character — the status of one recurrence instance slot.
#[derive(uniffi::Enum)]
pub enum FfiMaskChar {
    Pending,
    Waiting,
    Completed,
    Deleted,
    Unknown,
}

/// A `(index, epoch)` pair returned by `recurrence_diff`.
#[derive(uniffi::Record)]
pub struct FfiRecurrenceDiffEntry {
    /// Zero-based index into the due-dates array.
    pub index: u32,
    /// Due date as Unix epoch seconds.
    pub epoch: i64,
}

/// Input for `reconcile_ffi` — the current state of a recurring template.
///
/// Mirrors `praxis::recurrence::orchestrate::RecurringTemplate`.
#[derive(uniffi::Record)]
pub struct FfiRecurringTemplate {
    pub uuid: String,
    /// Initial due date as Unix epoch seconds.
    pub due_epoch: i64,
    /// Raw recurrence spec string (e.g. `"weekly"`, `"P1M"`).
    pub recur: String,
    /// Raw mask string (e.g. `"-+XW"`).
    pub mask: String,
    /// Hard expiry date as Unix epoch seconds. `None` if not set.
    pub until_epoch: Option<i64>,
    /// Wait date as Unix epoch seconds. `None` if not set.
    pub wait_epoch: Option<i64>,
    /// Fields to clone onto child tasks (description, project, tags, etc.).
    pub cloneable_fields: HashMap<String, String>,
}

/// Output of `reconcile_ffi` — actions the caller should execute against storage.
///
/// Mirrors `praxis::recurrence::orchestrate::RecurrenceAction`.
#[derive(uniffi::Enum)]
pub enum FfiRecurrenceAction {
    /// Create a child task instance.
    CreateChild {
        template_uuid: String,
        imask: u32,
        due_epoch: i64,
        wait_epoch: Option<i64>,
        cloneable_fields: HashMap<String, String>,
    },
    /// Update the template's mask string.
    UpdateTemplateMask {
        template_uuid: String,
        new_mask: String,
    },
    /// Template is fully expired — caller should mark it for deletion.
    ExpireTemplate { template_uuid: String },
    /// Generation hit the internal safety cap (10k iterations).
    ///
    /// Indicates corrupt or extreme recurrence data. Callers should log or
    /// surface this condition. Actions emitted before this warning are based
    /// on a partial date set.
    WarnHitLimit { template_uuid: String },
}

/// Input for `update_mask_for_child_ffi` — a child task status change.
///
/// Mirrors `praxis::recurrence::orchestrate::ChildStatusChange` (no `child_uuid` field).
#[derive(uniffi::Record)]
pub struct FfiChildStatusChange {
    /// UUID of the parent recurring template whose mask will be updated.
    pub template_uuid: String,
    /// Index into the parent mask for this child instance.
    pub imask: u32,
    /// New status for this child.
    pub new_status: FfiStatus,
    /// True when the child has a future `wait` date (logically "waiting").
    pub has_wait: bool,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn ffi_to_recurring_template(t: FfiRecurringTemplate) -> Result<RecurringTemplate, FfiError> {
    let uuid = parse_uuid_ctx(&t.uuid, "template UUID")?;
    let due = epoch_to_dt(t.due_epoch)?;
    let until = t.until_epoch.map(epoch_to_dt).transpose()?;
    let wait = t.wait_epoch.map(epoch_to_dt).transpose()?;
    Ok(RecurringTemplate {
        uuid,
        due,
        recur: t.recur,
        mask: t.mask,
        until,
        wait,
        cloneable_fields: t.cloneable_fields,
    })
}

fn recurrence_action_to_ffi(a: RecurrenceAction) -> FfiRecurrenceAction {
    match a {
        RecurrenceAction::CreateChild {
            template_uuid,
            imask,
            due,
            wait,
            cloneable_fields,
        } => FfiRecurrenceAction::CreateChild {
            template_uuid: template_uuid.to_string(),
            // Narrowing cast: praxis enforces a 10k iteration cap so this
            // will never exceed u32::MAX in practice, but we assert rather
            // than silently truncate.
            imask: imask
                .try_into()
                .expect("imask exceeds u32::MAX — bug in praxis"),
            due_epoch: due.timestamp(),
            wait_epoch: wait.map(|w| w.timestamp()),
            cloneable_fields,
        },
        RecurrenceAction::UpdateTemplateMask {
            template_uuid,
            new_mask,
        } => FfiRecurrenceAction::UpdateTemplateMask {
            template_uuid: template_uuid.to_string(),
            new_mask,
        },
        RecurrenceAction::ExpireTemplate { template_uuid } => FfiRecurrenceAction::ExpireTemplate {
            template_uuid: template_uuid.to_string(),
        },
        RecurrenceAction::WarnHitLimit { template_uuid } => FfiRecurrenceAction::WarnHitLimit {
            template_uuid: template_uuid.to_string(),
        },
    }
}

fn ffi_to_child_status_change(c: FfiChildStatusChange) -> Result<ChildStatusChange, FfiError> {
    let template_uuid = parse_uuid_ctx(&c.template_uuid, "template UUID")?;
    Ok(ChildStatusChange {
        template_uuid,
        imask: c.imask as usize, // widening cast: u32 → usize, always safe on 32/64-bit targets
        new_status: Status::from(c.new_status),
        has_wait: c.has_wait,
    })
}

// ---------------------------------------------------------------------------
// Exported functions
// ---------------------------------------------------------------------------

/// Parse a recurrence spec string (e.g. `"monthly"`, `"7d"`, `"P3W"`).
#[uniffi::export]
pub fn parse_recurrence_spec(input: String) -> Result<FfiRecurrenceSpec, FfiError> {
    parse_spec(&input)
        .map(FfiRecurrenceSpec::from)
        .map_err(|e| FfiError::InvalidInput {
            message: e.to_string(),
        })
}

/// Generate due dates for a recurrence template.
///
/// - `base_due_epoch`: the initial due date (Unix epoch seconds)
/// - `now_epoch`: current time (Unix epoch seconds); dates up to `future_limit`
///   instances beyond `now` are included
/// - `until_epoch`: optional hard stop (Unix epoch seconds)
/// - `future_limit`: maximum number of future instances to generate
#[uniffi::export]
pub fn generate_due_dates(
    spec: FfiRecurrenceSpec,
    base_due_epoch: i64,
    now_epoch: i64,
    until_epoch: Option<i64>,
    future_limit: u32,
) -> Result<FfiGeneratedDates, FfiError> {
    use chrono::DateTime;
    use praxis::recurrence::generate::generate_due_dates as praxis_generate;

    let base_due = epoch_to_dt(base_due_epoch)?;
    let now = epoch_to_dt(now_epoch)?;
    let until = until_epoch.map(epoch_to_dt).transpose()?;
    let rust_spec = RecurrenceSpec::from(spec);

    let result = praxis_generate(&rust_spec, base_due, now, until, future_limit as usize);

    Ok(FfiGeneratedDates {
        dates: result.dates.iter().map(DateTime::timestamp).collect(),
        until_reached: result.until_reached,
        hit_limit: result.hit_limit,
    })
}

/// Compute which recurrence instances still need to be created.
///
/// Returns `(index, epoch)` pairs for slots not yet covered by the mask.
#[uniffi::export]
pub fn recurrence_diff_ffi(
    mask: String,
    due_date_epochs: Vec<i64>,
) -> Result<Vec<FfiRecurrenceDiffEntry>, FfiError> {
    let parsed_mask = parse_mask(&mask);
    let dates: Result<Vec<_>, _> = due_date_epochs.iter().map(|&e| epoch_to_dt(e)).collect();
    let dates = dates?;

    let diff = recurrence_diff(&parsed_mask, &dates);
    Ok(diff
        .into_iter()
        .map(|(i, dt)| FfiRecurrenceDiffEntry {
            index: i as u32,
            epoch: dt.timestamp(),
        })
        .collect())
}

/// Map a task's FFI status and wait state to the appropriate mask character.
#[uniffi::export]
pub fn mask_char_for_ffi_status(status: FfiStatus, has_wait: bool) -> FfiMaskChar {
    use praxis::recurrence::mask::MaskChar;

    let tc_status = Status::from(status);
    match mask_char_for_status(&tc_status, has_wait) {
        MaskChar::Pending => FfiMaskChar::Pending,
        MaskChar::Waiting => FfiMaskChar::Waiting,
        MaskChar::Completed => FfiMaskChar::Completed,
        MaskChar::Deleted => FfiMaskChar::Deleted,
        MaskChar::Unknown => FfiMaskChar::Unknown,
    }
}

/// Check whether the recurrence template has fully expired.
///
/// Pass the serialized mask string, the total number of generated due dates,
/// and whether the `until` boundary was reached.
///
/// This function is infallible: `parse_mask` and `is_template_expired` both
/// have no error paths, so the return type is a plain `bool`.
#[uniffi::export]
pub fn is_template_expired_ffi(mask: String, due_count: u32, until_reached: bool) -> bool {
    use praxis::recurrence::mask::is_template_expired;

    let parsed_mask = parse_mask(&mask);
    is_template_expired(&parsed_mask, due_count as usize, until_reached)
}

/// Orchestrate recurrence: given templates and the current time, return all
/// actions needed to bring task storage up to date.
///
/// `future_limit` limits how many future (not-yet-due) instances are
/// pre-generated per template (matches TW's `recurrence.limit` config, default 1).
///
/// Returns `InvalidInput` if any template's `recur` field cannot be parsed.
#[uniffi::export]
pub fn reconcile_ffi(
    templates: Vec<FfiRecurringTemplate>,
    now_epoch: i64,
    future_limit: u32,
) -> Result<Vec<FfiRecurrenceAction>, FfiError> {
    use praxis::recurrence::orchestrate::reconcile;

    let now = epoch_to_dt(now_epoch)?;
    let rust_templates: Result<Vec<_>, _> = templates
        .into_iter()
        .map(ffi_to_recurring_template)
        .collect();
    let rust_templates = rust_templates?;

    let actions = reconcile(&rust_templates, now, future_limit as usize).map_err(|e| {
        FfiError::InvalidInput {
            message: e.to_string(),
        }
    })?;

    Ok(actions.into_iter().map(recurrence_action_to_ffi).collect())
}

/// Update a template's mask when a child task's status changes.
///
/// `current_mask` is the raw mask string (e.g. `"-+-"`).
/// Returns the updated mask string, or `InvalidInput` if `imask` is out of
/// bounds for the mask.
#[uniffi::export]
pub fn update_mask_for_child_ffi(
    current_mask: String,
    change: FfiChildStatusChange,
) -> Result<String, FfiError> {
    use praxis::recurrence::orchestrate::update_mask_for_child;

    let parsed_mask = parse_mask(&current_mask);
    let rust_change = ffi_to_child_status_change(change)?;
    update_mask_for_child(&parsed_mask, &rust_change).map_err(|e| FfiError::InvalidInput {
        message: e.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn epoch_to_dt(epoch: i64) -> Result<chrono::DateTime<chrono::Utc>, FfiError> {
    chrono::DateTime::from_timestamp(epoch, 0).ok_or_else(|| FfiError::InvalidInput {
        message: format!("invalid epoch timestamp: {epoch}"),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn uuid_str() -> String {
        "12345678-1234-1234-1234-123456789abc".to_string()
    }

    fn template_uuid_str() -> String {
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string()
    }

    fn dt_epoch(year: i32, month: u32, day: u32) -> i64 {
        chrono::Utc
            .with_ymd_and_hms(year, month, day, 0, 0, 0)
            .unwrap()
            .timestamp()
    }

    // --- FfiRecurringTemplate → RecurringTemplate conversion ---

    #[test]
    fn ffi_recurring_template_roundtrip_basic() {
        let due = dt_epoch(2025, 1, 1);
        let ffi = FfiRecurringTemplate {
            uuid: uuid_str(),
            due_epoch: due,
            recur: "weekly".to_string(),
            mask: "-+-".to_string(),
            until_epoch: None,
            wait_epoch: None,
            cloneable_fields: HashMap::new(),
        };
        let rust = ffi_to_recurring_template(ffi).unwrap();
        assert_eq!(rust.uuid.to_string(), uuid_str());
        assert_eq!(rust.due.timestamp(), due);
        assert_eq!(rust.recur, "weekly");
        assert_eq!(rust.mask, "-+-");
        assert!(rust.until.is_none());
        assert!(rust.wait.is_none());
        assert!(rust.cloneable_fields.is_empty());
    }

    #[test]
    fn ffi_recurring_template_with_until_and_wait() {
        let due = dt_epoch(2025, 1, 1);
        let until = dt_epoch(2025, 12, 31);
        let wait = dt_epoch(2024, 12, 31);
        let ffi = FfiRecurringTemplate {
            uuid: uuid_str(),
            due_epoch: due,
            recur: "monthly".to_string(),
            mask: "".to_string(),
            until_epoch: Some(until),
            wait_epoch: Some(wait),
            cloneable_fields: HashMap::new(),
        };
        let rust = ffi_to_recurring_template(ffi).unwrap();
        assert_eq!(rust.until.unwrap().timestamp(), until);
        assert_eq!(rust.wait.unwrap().timestamp(), wait);
    }

    #[test]
    fn ffi_recurring_template_cloneable_fields_passthrough() {
        let mut fields = HashMap::new();
        fields.insert("project".to_string(), "inbox".to_string());
        fields.insert("description".to_string(), "Pay bills".to_string());
        let ffi = FfiRecurringTemplate {
            uuid: uuid_str(),
            due_epoch: dt_epoch(2025, 1, 1),
            recur: "monthly".to_string(),
            mask: "".to_string(),
            until_epoch: None,
            wait_epoch: None,
            cloneable_fields: fields.clone(),
        };
        let rust = ffi_to_recurring_template(ffi).unwrap();
        assert_eq!(rust.cloneable_fields, fields);
    }

    #[test]
    fn ffi_recurring_template_invalid_uuid() {
        let ffi = FfiRecurringTemplate {
            uuid: "not-a-uuid".to_string(),
            due_epoch: dt_epoch(2025, 1, 1),
            recur: "weekly".to_string(),
            mask: "".to_string(),
            until_epoch: None,
            wait_epoch: None,
            cloneable_fields: HashMap::new(),
        };
        assert!(matches!(
            ffi_to_recurring_template(ffi),
            Err(FfiError::InvalidInput { .. })
        ));
    }

    // --- RecurrenceAction → FfiRecurrenceAction conversion ---

    #[test]
    fn recurrence_action_create_child_to_ffi() {
        let template_uuid = Uuid::parse_str(&template_uuid_str()).unwrap();
        let due = chrono::Utc.with_ymd_and_hms(2025, 2, 1, 0, 0, 0).unwrap();
        let mut fields = HashMap::new();
        fields.insert("k".to_string(), "v".to_string());
        let action = RecurrenceAction::CreateChild {
            template_uuid,
            imask: 3usize,
            due,
            wait: None,
            cloneable_fields: fields.clone(),
        };
        let ffi = recurrence_action_to_ffi(action);
        match ffi {
            FfiRecurrenceAction::CreateChild {
                template_uuid: t,
                imask,
                due_epoch,
                wait_epoch,
                cloneable_fields,
            } => {
                assert_eq!(t, template_uuid_str());
                assert_eq!(imask, 3u32);
                assert_eq!(due_epoch, due.timestamp());
                assert!(wait_epoch.is_none());
                assert_eq!(cloneable_fields, fields);
            }
            _ => panic!("expected CreateChild"),
        }
    }

    #[test]
    fn recurrence_action_update_mask_to_ffi() {
        let template_uuid = Uuid::parse_str(&template_uuid_str()).unwrap();
        let action = RecurrenceAction::UpdateTemplateMask {
            template_uuid,
            new_mask: "+-+".to_string(),
        };
        let ffi = recurrence_action_to_ffi(action);
        match ffi {
            FfiRecurrenceAction::UpdateTemplateMask {
                template_uuid: t,
                new_mask,
            } => {
                assert_eq!(t, template_uuid_str());
                assert_eq!(new_mask, "+-+");
            }
            _ => panic!("expected UpdateTemplateMask"),
        }
    }

    #[test]
    fn recurrence_action_expire_to_ffi() {
        let template_uuid = Uuid::parse_str(&template_uuid_str()).unwrap();
        let action = RecurrenceAction::ExpireTemplate { template_uuid };
        let ffi = recurrence_action_to_ffi(action);
        assert!(matches!(ffi, FfiRecurrenceAction::ExpireTemplate { .. }));
    }

    #[test]
    fn recurrence_action_warn_hit_limit_to_ffi() {
        let template_uuid = Uuid::parse_str(&template_uuid_str()).unwrap();
        let action = RecurrenceAction::WarnHitLimit { template_uuid };
        let ffi = recurrence_action_to_ffi(action);
        assert!(matches!(ffi, FfiRecurrenceAction::WarnHitLimit { .. }));
    }

    // --- FfiChildStatusChange → ChildStatusChange conversion ---

    #[test]
    fn ffi_child_status_change_roundtrip() {
        let ffi = FfiChildStatusChange {
            template_uuid: template_uuid_str(),
            imask: 2u32,
            new_status: FfiStatus::Completed,
            has_wait: false,
        };
        let rust = ffi_to_child_status_change(ffi).unwrap();
        assert_eq!(rust.template_uuid.to_string(), template_uuid_str());
        assert_eq!(rust.imask, 2usize);
        assert!(matches!(rust.new_status, Status::Completed));
        assert!(!rust.has_wait);
    }

    #[test]
    fn ffi_child_status_change_invalid_uuid() {
        let ffi = FfiChildStatusChange {
            template_uuid: "bad".to_string(),
            imask: 0,
            new_status: FfiStatus::Pending,
            has_wait: false,
        };
        assert!(matches!(
            ffi_to_child_status_change(ffi),
            Err(FfiError::InvalidInput { .. })
        ));
    }

    // --- reconcile_ffi ---

    #[test]
    fn reconcile_one_template_empty_mask_creates_child() {
        // Template with due in the past, empty mask → should create at least one child
        let now = dt_epoch(2025, 6, 1);
        let template = FfiRecurringTemplate {
            uuid: uuid_str(),
            due_epoch: dt_epoch(2025, 5, 1),
            recur: "monthly".to_string(),
            mask: "".to_string(),
            until_epoch: None,
            wait_epoch: None,
            cloneable_fields: HashMap::new(),
        };
        let actions = reconcile_ffi(vec![template], now, 1).unwrap();
        // At minimum: CreateChild for the past-due instance + UpdateTemplateMask
        assert!(!actions.is_empty());
        // Verify CreateChild action carries the correct template UUID
        let create = actions
            .iter()
            .find(|a| matches!(a, FfiRecurrenceAction::CreateChild { .. }))
            .expect("expected at least one CreateChild");
        if let FfiRecurrenceAction::CreateChild { template_uuid, .. } = create {
            assert_eq!(template_uuid, &uuid_str());
        }
        assert!(actions
            .iter()
            .any(|a| matches!(a, FfiRecurrenceAction::UpdateTemplateMask { .. })));
    }

    #[test]
    fn reconcile_invalid_recur_spec_returns_invalid_input() {
        let now = dt_epoch(2025, 6, 1);
        let template = FfiRecurringTemplate {
            uuid: uuid_str(),
            due_epoch: dt_epoch(2025, 5, 1),
            recur: "not-a-valid-spec".to_string(),
            mask: "".to_string(),
            until_epoch: None,
            wait_epoch: None,
            cloneable_fields: HashMap::new(),
        };
        let result = reconcile_ffi(vec![template], now, 1);
        assert!(matches!(result, Err(FfiError::InvalidInput { .. })));
    }

    // --- update_mask_for_child_ffi ---

    #[test]
    fn update_mask_for_child_ffi_completed() {
        // mask "-+-"; set index 0 to Completed → "++-"
        let change = FfiChildStatusChange {
            template_uuid: template_uuid_str(),
            imask: 0,
            new_status: FfiStatus::Completed,
            has_wait: false,
        };
        let result = update_mask_for_child_ffi("-+-".to_string(), change).unwrap();
        assert_eq!(result, "++-");
    }

    #[test]
    fn update_mask_for_child_ffi_deleted() {
        // mask "-+W"; set index 0 to Deleted → "X+W"
        let change = FfiChildStatusChange {
            template_uuid: template_uuid_str(),
            imask: 0,
            new_status: FfiStatus::Deleted,
            has_wait: false,
        };
        let result = update_mask_for_child_ffi("-+W".to_string(), change).unwrap();
        assert_eq!(result, "X+W");
    }

    #[test]
    fn update_mask_for_child_ffi_pending_has_wait() {
        // mask "-+"; set index 0 to Pending with wait → "W+"
        let change = FfiChildStatusChange {
            template_uuid: template_uuid_str(),
            imask: 0,
            new_status: FfiStatus::Pending,
            has_wait: true,
        };
        let result = update_mask_for_child_ffi("-+".to_string(), change).unwrap();
        assert_eq!(result, "W+");
    }

    #[test]
    fn update_mask_for_child_ffi_out_of_bounds() {
        let change = FfiChildStatusChange {
            template_uuid: template_uuid_str(),
            imask: 99, // way out of bounds
            new_status: FfiStatus::Completed,
            has_wait: false,
        };
        let result = update_mask_for_child_ffi("-+-".to_string(), change);
        assert!(matches!(result, Err(FfiError::InvalidInput { .. })));
    }
}
