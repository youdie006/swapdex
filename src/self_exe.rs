//! Subcommands launched by the persistent picker must explain stale installs.

use std::io;
use std::process::{Command, Output, Stdio};

fn launch_error(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::NotFound {
        io::Error::new(
            io::ErrorKind::NotFound,
            "close this screen and reopen swapdex; its executable is unavailable",
        )
    } else {
        error
    }
}

/// npm can unlink this process's executable while its picker stays open. Map
/// the actual spawn error, including an unlink between lookup and execution.
/// Never search PATH or revive the removed executable through /proc.
pub(crate) fn output(args: &[&str]) -> io::Result<Output> {
    let executable = std::env::current_exe().map_err(launch_error)?;
    Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(launch_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrelated_launch_errors_keep_their_cause() {
        let error = launch_error(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "denied");
    }
}
