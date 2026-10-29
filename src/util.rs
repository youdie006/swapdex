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

/// ANSI colour is used only on a TTY and never when NO_COLOR is set
/// (https://no-color.org).
pub fn color_enabled() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// An account name as it has to be typed into a shell, for the commands the
/// hints print. A name may hold spaces and quotes, and `swapdex run my work`
/// pasted as printed made an account called "my". Plain names stay bare.
pub fn shell_word(s: &str) -> std::borrow::Cow<'_, str> {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '+' | ':' | ','));
    if plain {
        s.into()
    } else {
        format!("'{}'", s.replace('\'', r"'\''")).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_name_stays_bare_and_anything_else_is_quoted() {
        for plain in ["work", "a.b@x.com", "team-2", "업무"] {
            assert_eq!(shell_word(plain), plain);
        }
        assert_eq!(shell_word("my work"), "'my work'");
        assert_eq!(shell_word("bob's"), r"'bob'\''s'");
        // Expanded by some shell at the start of a word, so never bare.
        for odd in ["~me", "=me", "$me", "a*", "#me", "!me", "%me"] {
            assert_eq!(shell_word(odd), format!("'{odd}'"));
        }
    }
}
