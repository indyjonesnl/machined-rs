//! Last-resort handling when boot fails. In PID-1 context a bare exit would
//! panic the kernel, so we log loudly and (optionally) reboot. For M1 we log
//! and return; the caller decides whether to halt.

use machined_platform::Platform;
use std::sync::Arc;
use tracing::error;

/// Log a fatal boot error. If `reboot_on_failure` is set, ask the platform to
/// reboot; otherwise return so the caller can park.
pub fn enter_emergency(platform: &Arc<dyn Platform>, err: &dyn std::fmt::Display, reboot_on_failure: bool) {
    error!("FATAL during boot: {err}");
    error!("entering emergency state");
    if reboot_on_failure {
        if let Err(e) = platform.reboot() {
            error!("emergency reboot failed: {e}");
        }
    }
}
