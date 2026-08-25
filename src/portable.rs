//! Move an account SETUP to another machine - never the logins.
//!
//! Setting up a second machine means re-creating the same accounts by hand:
//! the names, which tool each belongs to, the rotation settings. That is
//! tedious and easy to get subtly wrong, and a name that differs between two
//! machines makes every note about "serve work" wrong on one of them.
//!
//! What travels is the SHAPE: names, tools, preferences. What never travels is a
//! credential. Not because it would be hard - because a file that can carry a
//! token is a file that will eventually be pasted into a chat window, and this
//! project exists to keep logins where they are.

use crate::paths::Paths;
use serde::{Deserialize, Serialize};

/// One account, as it travels: a name and which tool it belongs to. The
/// directory does NOT travel - it is a path on one machine, and the other one
/// will make its own.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PortableAccount {
    pub name: String,
    pub tool: String,
}

/// The whole setup, minus every secret.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Portable {
    /// Bumped only if the shape changes in a way an older swapdex would misread.
    pub version: u32,
    pub accounts: Vec<PortableAccount>,
    /// Tools whose registry could not be read, so this manifest is incomplete.
    /// Absent when everything was readable, so an older reader is unaffected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<crate::settings::Settings>,
}

pub const FORMAT_VERSION: u32 = 1;

/// Gather what can safely leave this machine.
pub fn export(paths: &Paths) -> Portable {
    let mut accounts = Vec::new();
    // A registry that will not parse is not an empty one. Skipping it silently
    // hands back a manifest that looks complete and is not, which is the one
    // thing an export must never do.
    let mut unreadable = Vec::new();
    for tool in crate::adapters::names() {
        if crate::slots::Slots::open_for(paths, tool).is_err() {
            unreadable.push(tool.to_string());
        }
        if let Ok(s) = crate::slots::Slots::open_for(paths, tool) {
            for r in s.list() {
                accounts.push(PortableAccount {
                    name: r.name,
                    tool: tool.to_string(),
                });
            }
        }
    }
    Portable {
        version: FORMAT_VERSION,
        unreadable,
        accounts,
        settings: Some(crate::settings::load(paths)),
    }
}

/// What an import WOULD do, without doing it. Returns the accounts that would be
/// created; ones already here are left alone, because a local account is the one
/// with a login in it and the incoming file has none.
pub fn plan<'a>(here: &[(String, String)], incoming: &'a Portable) -> Vec<&'a PortableAccount> {
    incoming
        .accounts
        .iter()
        .filter(|a| !here.iter().any(|(n, t)| n == &a.name && t == &a.tool))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test that matters most. Everything else here is convenience; this is
    /// the one that keeps a token out of a file people will email to themselves.
    #[test]
    fn nothing_a_credential_could_hide_in_survives_the_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let rec = crate::slots::Slots::open_for(&paths, "claude-code")
            .unwrap()
            .create("work")
            .unwrap();
        std::fs::write(
            rec.config_dir.join(".credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"sk-ant-SECRET","refreshToken":"rt-SECRET"}}"#,
        )
        .unwrap();

        let text = serde_json::to_string(&export(&paths)).unwrap();
        for forbidden in [
            "sk-ant",
            "SECRET",
            "accessToken",
            "refreshToken",
            "claudeAiOauth",
        ] {
            assert!(
                !text.contains(forbidden),
                "{forbidden} reached the export: {text}"
            );
        }
        // Nor the paths, which name the machine and the person.
        assert!(!text.contains(&rec.config_dir.display().to_string()));
    }

    #[test]
    fn the_names_and_their_tools_travel() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        crate::slots::Slots::open_for(&paths, "claude-code")
            .unwrap()
            .create("work")
            .unwrap();
        crate::slots::Slots::open_for(&paths, "codex")
            .unwrap()
            .create("work")
            .unwrap();
        let p = export(&paths);
        assert_eq!(p.version, FORMAT_VERSION);
        assert_eq!(
            p.accounts.len(),
            2,
            "same name on two tools is two accounts"
        );
        assert!(p.accounts.iter().any(|a| a.tool == "codex"));
    }

    /// An account already here keeps whatever it has - the incoming file has no
    /// login to offer, so overwriting would only lose one.
    #[test]
    fn an_account_that_already_exists_is_left_alone() {
        let incoming = Portable {
            unreadable: Vec::new(),
            version: FORMAT_VERSION,
            accounts: vec![
                PortableAccount {
                    name: "work".into(),
                    tool: "claude-code".into(),
                },
                PortableAccount {
                    name: "new".into(),
                    tool: "claude-code".into(),
                },
                PortableAccount {
                    name: "work".into(),
                    tool: "codex".into(),
                },
            ],
            settings: None,
        };
        let here = [("work".to_string(), "claude-code".to_string())];
        let todo = plan(&here, &incoming);
        let names: Vec<&str> = todo.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            ["new", "work"],
            "the codex 'work' is a different account"
        );
    }
}
