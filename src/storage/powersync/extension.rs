use std::ffi::c_int;
use std::sync::OnceLock;

use crate::errors::{Error, Result};

// The PowerSync C extension is statically linked via powersync_sqlite_nostd.
// We only need one entry point: powersync_init_static() registers the extension
// as a SQLite auto-extension so it fires on every subsequent Connection::open().
unsafe extern "C" {
    fn powersync_init_static() -> c_int;
}

static POWERSYNC_RC: OnceLock<c_int> = OnceLock::new();

/// Register the PowerSync SQLite auto-extension exactly once per process.
/// Returns `Err` if registration failed at startup; subsequent calls return the same
/// cached error — the failure is permanent within the process lifetime.
pub(super) fn init_powersync_extension() -> Result<()> {
    // SAFETY: powersync_init_static calls sqlite3_auto_extension.
    // OnceLock guarantees the closure runs exactly once, so there is no concurrent call.
    let rc = POWERSYNC_RC.get_or_init(|| unsafe { powersync_init_static() });
    if *rc != 0 {
        return Err(Error::Database(format!(
            "PowerSync extension failed to initialize at startup (rc={rc}); process restart required"
        )));
    }
    Ok(())
}
