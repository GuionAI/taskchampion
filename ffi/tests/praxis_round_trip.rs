//! Round-trip tests for the praxis FFI bridge.

use taskchampion_ffi::recurrence::{
    generate_due_dates, is_template_expired_ffi, mask_char_for_ffi_status, parse_recurrence_spec,
    recurrence_diff_ffi, FfiMaskChar, FfiRecurrenceDiffEntry, FfiRecurrenceSpec,
};
use taskchampion_ffi::tree::{
    descendants_to_complete_ffi, descendants_to_delete_ffi, FfiTaskDescendant,
};
use taskchampion_ffi::types::FfiStatus;

// ---------------------------------------------------------------------------
// parse_recurrence_spec
// ---------------------------------------------------------------------------

#[test]
fn test_parse_spec_round_trip() {
    // Named period
    let spec = parse_recurrence_spec("monthly".to_string()).unwrap();
    assert!(matches!(spec, FfiRecurrenceSpec::Monthly));

    // Shorthand: "7d" → NDays { n: 7 }
    let spec = parse_recurrence_spec("7d".to_string()).unwrap();
    assert!(matches!(spec, FfiRecurrenceSpec::NDays { n: 7 }));

    // ISO: "P3W" → IsoWeeks { n: 3 }
    let spec = parse_recurrence_spec("P3W".to_string()).unwrap();
    assert!(matches!(spec, FfiRecurrenceSpec::IsoWeeks { n: 3 }));

    // Seconds fallback
    let spec = parse_recurrence_spec("86400".to_string()).unwrap();
    assert!(matches!(spec, FfiRecurrenceSpec::Seconds { secs: 86400 }));
}

#[test]
fn test_parse_spec_errors() {
    assert!(parse_recurrence_spec("".to_string()).is_err());
    assert!(parse_recurrence_spec("gibberish".to_string()).is_err());
    assert!(parse_recurrence_spec("P0M".to_string()).is_err()); // zero period
}

// ---------------------------------------------------------------------------
// generate_due_dates
// ---------------------------------------------------------------------------

#[test]
fn test_generate_due_dates_monthly() {
    // Base due: 2024-01-15 UTC (epoch: 2024-01-15 00:00:00 UTC)
    let base_epoch: i64 = 1705276800; // 2024-01-15 00:00:00 UTC
    let now_epoch: i64 = 1705276800; // same — all future
    let spec = FfiRecurrenceSpec::Monthly;

    let result = generate_due_dates(spec, base_epoch, now_epoch, None, 3).unwrap();
    // Should generate 1 past (base) + 3 future = at least 1 date at base_epoch
    assert!(!result.dates.is_empty());
    // First date should be the base
    assert_eq!(result.dates[0], base_epoch);
    assert!(!result.hit_limit);
}

// ---------------------------------------------------------------------------
// recurrence_diff_ffi
// ---------------------------------------------------------------------------

#[test]
fn test_recurrence_diff_round_trip() {
    // mask "-+" covers indices 0 and 1; index 2 needs generation
    let base: i64 = 1700000000;
    let dates = vec![base, base + 86400, base + 172800];
    let diff = recurrence_diff_ffi("-+".to_string(), dates).unwrap();

    assert_eq!(diff.len(), 1);
    let FfiRecurrenceDiffEntry { index, epoch } = &diff[0];
    assert_eq!(*index, 2u32);
    assert_eq!(*epoch, base + 172800);
}

#[test]
fn test_recurrence_diff_empty_mask() {
    let base: i64 = 1700000000;
    let dates = vec![base, base + 86400];
    let diff = recurrence_diff_ffi("".to_string(), dates).unwrap();
    assert_eq!(diff.len(), 2);
    assert_eq!(diff[0].index, 0);
    assert_eq!(diff[1].index, 1);
}

// ---------------------------------------------------------------------------
// mask_char_for_ffi_status
// ---------------------------------------------------------------------------

#[test]
fn test_mask_char_for_status() {
    // Recurring → Unknown
    let c = mask_char_for_ffi_status(FfiStatus::Recurring, false);
    assert!(matches!(c, FfiMaskChar::Unknown));

    // Pending + has_wait:true → Waiting
    let c = mask_char_for_ffi_status(FfiStatus::Pending, true);
    assert!(matches!(c, FfiMaskChar::Waiting));

    // Pending + has_wait:false → Pending
    let c = mask_char_for_ffi_status(FfiStatus::Pending, false);
    assert!(matches!(c, FfiMaskChar::Pending));

    // Completed → Completed
    let c = mask_char_for_ffi_status(FfiStatus::Completed, false);
    assert!(matches!(c, FfiMaskChar::Completed));

    // Deleted → Deleted
    let c = mask_char_for_ffi_status(FfiStatus::Deleted, false);
    assert!(matches!(c, FfiMaskChar::Deleted));
}

// ---------------------------------------------------------------------------
// is_template_expired_ffi
// ---------------------------------------------------------------------------

#[test]
fn test_is_template_expired() {
    // All completed + until_reached → expired
    assert!(is_template_expired_ffi("++X".to_string(), 3, true).unwrap());

    // Has pending slot → NOT expired
    assert!(!is_template_expired_ffi("+-".to_string(), 2, true).unwrap());

    // until_reached = false → NOT expired
    assert!(!is_template_expired_ffi("++".to_string(), 2, false).unwrap());

    // Mask shorter than due_count → NOT expired
    assert!(!is_template_expired_ffi("++".to_string(), 3, true).unwrap());

    // Unknown '?' entries don't block expiry
    assert!(is_template_expired_ffi("+?X".to_string(), 3, true).unwrap());
}

// ---------------------------------------------------------------------------
// descendants_to_complete_ffi
// ---------------------------------------------------------------------------

fn make_desc(uuid: &str, status: FfiStatus, has_wait: bool) -> FfiTaskDescendant {
    FfiTaskDescendant {
        uuid: uuid.to_string(),
        status,
        has_wait,
    }
}

const UUID1: &str = "00000000-0000-0000-0000-000000000001";
const UUID2: &str = "00000000-0000-0000-0000-000000000002";
const UUID3: &str = "00000000-0000-0000-0000-000000000003";
const UUID4: &str = "00000000-0000-0000-0000-000000000004";

#[test]
fn test_descendants_to_complete() {
    let descendants = vec![
        make_desc(UUID1, FfiStatus::Pending, false), // pending → complete
        make_desc(UUID2, FfiStatus::Pending, true),  // waiting → complete
        make_desc(UUID3, FfiStatus::Completed, false), // skip
        make_desc(UUID4, FfiStatus::Deleted, false), // skip
    ];
    let result = descendants_to_complete_ffi(descendants).unwrap();
    assert_eq!(result.len(), 2);
    assert!(result.contains(&UUID1.to_string()));
    assert!(result.contains(&UUID2.to_string()));
    assert!(!result.contains(&UUID3.to_string()));
    assert!(!result.contains(&UUID4.to_string()));
}

#[test]
fn test_descendants_to_complete_skips_recurring_and_unknown() {
    let descendants = vec![
        make_desc(UUID1, FfiStatus::Recurring, false),
        make_desc(
            UUID2,
            FfiStatus::Unknown {
                value: "custom".to_string(),
            },
            false,
        ),
    ];
    let result = descendants_to_complete_ffi(descendants).unwrap();
    assert!(result.is_empty());
}

// ---------------------------------------------------------------------------
// descendants_to_delete_ffi
// ---------------------------------------------------------------------------

#[test]
fn test_descendants_to_delete() {
    let descendants = vec![
        make_desc(UUID1, FfiStatus::Pending, false),   // pending
        make_desc(UUID2, FfiStatus::Pending, true),    // waiting (counts as pending)
        make_desc(UUID3, FfiStatus::Completed, false), // not pending
        make_desc(UUID4, FfiStatus::Deleted, false),   // not pending
    ];
    let result = descendants_to_delete_ffi(descendants).unwrap();
    assert_eq!(result.pending_count, 2);
    assert_eq!(result.all_uuids.len(), 4);
    assert!(result.all_uuids.contains(&UUID1.to_string()));
    assert!(result.all_uuids.contains(&UUID2.to_string()));
    assert!(result.all_uuids.contains(&UUID3.to_string()));
    assert!(result.all_uuids.contains(&UUID4.to_string()));
}

#[test]
fn test_descendants_to_delete_unknown_not_counted() {
    let descendants = vec![
        make_desc(UUID1, FfiStatus::Recurring, false),
        make_desc(
            UUID2,
            FfiStatus::Unknown {
                value: "x".to_string(),
            },
            false,
        ),
    ];
    let result = descendants_to_delete_ffi(descendants).unwrap();
    assert_eq!(result.pending_count, 0);
    assert_eq!(result.all_uuids.len(), 2);
}
