//! Attribute sessions to the account that was active when they ran, by joining
//! the switch `timeline` with session start times. Filled in Task 9; the
//! `status_line` hook is the ecosystem one-liner for `status`.

use crate::paths::Paths;

/// A one-line "N sessions across M accounts" for `status`, or None if the data
/// is unavailable. Best-effort; never blocks or errors the caller.
pub fn status_line(_paths: &Paths) -> Option<String> {
    None
}
