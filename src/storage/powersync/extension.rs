use std::ffi::c_int;
use std::sync::OnceLock;

use crate::errors::{Error, Result};

static POWERSYNC_RC: OnceLock<c_int> = OnceLock::new();

/// Register the PowerSync SQLite auto-extension exactly once per process.
/// Returns `Err` if registration failed at startup; subsequent calls return the same
/// cached error — the failure is permanent within the process lifetime.
pub(super) fn init_powersync_extension() -> Result<()> {
    let rc = POWERSYNC_RC.get_or_init(|| powersync_core::powersync_init_static());
    if *rc != 0 {
        return Err(Error::Database(format!(
            "PowerSync extension failed to initialize at startup (rc={rc}); process restart required"
        )));
    }
    Ok(())
}
