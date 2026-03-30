//! FFI types and exported functions for praxis tree behavior.

use crate::recurrence::FfiDeleteResult;
use crate::types::{FfiError, FfiStatus};
use praxis::tree::behavior::{descendants_to_complete, descendants_to_delete, TaskDescendant};
use taskchampion::Status;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// FFI types
// ---------------------------------------------------------------------------

/// A task in the tree hierarchy — status info for cascade operations.
#[derive(uniffi::Record)]
pub struct FfiTaskDescendant {
    /// Task UUID as a string.
    pub uuid: String,
    /// Task status.
    pub status: FfiStatus,
    /// True when the task has a future `wait` date (logically "waiting").
    pub has_wait: bool,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ffi_to_task_descendant(d: FfiTaskDescendant) -> Result<TaskDescendant, FfiError> {
    let uuid = Uuid::parse_str(&d.uuid).map_err(|e| FfiError::InvalidInput {
        message: format!("invalid UUID '{}': {e}", d.uuid),
    })?;
    Ok(TaskDescendant {
        uuid,
        status: Status::from(d.status),
        has_wait: d.has_wait,
    })
}

// ---------------------------------------------------------------------------
// Exported functions
// ---------------------------------------------------------------------------

/// Return UUIDs of descendants that should be auto-completed when the parent
/// is completed.
///
/// Only Pending (and Waiting) descendants are returned — Completed, Deleted,
/// Recurring, and Unknown are skipped.
#[uniffi::export]
pub fn descendants_to_complete_ffi(
    descendants: Vec<FfiTaskDescendant>,
) -> Result<Vec<String>, FfiError> {
    let rust_descs: Result<Vec<_>, _> = descendants
        .into_iter()
        .map(ffi_to_task_descendant)
        .collect();
    let rust_descs = rust_descs?;
    Ok(descendants_to_complete(&rust_descs)
        .into_iter()
        .map(|u| u.to_string())
        .collect())
}

/// Return the pending count and all UUIDs when the parent task is deleted.
///
/// `pending_count` is the number of Pending/Waiting descendants — used to
/// decide whether to prompt the user. `all_uuids` contains every descendant
/// UUID regardless of status.
#[uniffi::export]
pub fn descendants_to_delete_ffi(
    descendants: Vec<FfiTaskDescendant>,
) -> Result<FfiDeleteResult, FfiError> {
    let rust_descs: Result<Vec<_>, _> = descendants
        .into_iter()
        .map(ffi_to_task_descendant)
        .collect();
    let rust_descs = rust_descs?;
    let (pending_count, all_uuids) = descendants_to_delete(&rust_descs);
    Ok(FfiDeleteResult {
        pending_count: pending_count as u32,
        all_uuids: all_uuids.into_iter().map(|u| u.to_string()).collect(),
    })
}
