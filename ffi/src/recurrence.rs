//! FFI types and exported functions for praxis recurrence support.

use crate::types::{FfiError, FfiStatus};
use praxis::recurrence::mask::{mask_char_for_status, parse_mask, recurrence_diff};
use praxis::recurrence::spec::{parse_spec, RecurrenceSpec};
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn epoch_to_dt(epoch: i64) -> Result<chrono::DateTime<chrono::Utc>, FfiError> {
    chrono::DateTime::from_timestamp(epoch, 0).ok_or_else(|| FfiError::InvalidInput {
        message: format!("invalid epoch timestamp: {epoch}"),
    })
}
