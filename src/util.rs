//! Small helpers. `redact_path` replaces the home dir with `~` so no error or
//! status line leaks a username.

pub fn redact_path(s: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy();
        if !home.is_empty() {
            return s.replace(home.as_ref(), "~");
        }
    }
    s.to_string()
}
