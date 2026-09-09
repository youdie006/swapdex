//! The tag must reproduce what was published.

/// `npm/package.json` must carry the version `Cargo.toml` declares.
///
/// `npm/publish.mjs` rewrites this field from Cargo.toml at publish time, which
/// happens AFTER the release is committed and tagged - so every tag held the
/// PREVIOUS release's npm metadata. Checking out `v0.103.0` gave a tree that
/// would publish 0.102.0. Five tags in a row were off by one, and nothing said
/// so because the file is correct on disk the moment after publishing.
///
/// This runs in CI on the tag, so a release whose metadata does not match its
/// own version fails there instead of shipping.
#[test]
fn the_npm_manifest_carries_the_version_cargo_declares() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let toml = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let cargo = toml
        .lines()
        .find_map(|l| l.strip_prefix("version = \""))
        .and_then(|l| l.split('"').next())
        .expect("Cargo.toml declares a version");

    let pkg = std::fs::read_to_string(root.join("npm/package.json")).unwrap();
    let npm = pkg
        .lines()
        .find_map(|l| l.trim().strip_prefix("\"version\": \""))
        .and_then(|l| l.split('"').next())
        .expect("npm/package.json declares a version");

    assert_eq!(
        cargo, npm,
        "npm/package.json says {npm} while Cargo.toml says {cargo} - this tag \
         does not reproduce the release it names"
    );
}

/// The pinned platform packages must carry that version too.
///
/// `optionalDependencies` decides which binary an `npm i` actually fetches, so
/// it matters more than the version field beside it - and it was TWO releases
/// behind in the committed tree while the version field was one. A tag whose
/// manifest pins old platform packages installs an old swapdex no matter what
/// the version says.
#[test]
fn the_pinned_platform_packages_carry_that_version_too() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let toml = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let cargo = toml
        .lines()
        .find_map(|l| l.strip_prefix("version = \""))
        .and_then(|l| l.split('"').next())
        .expect("Cargo.toml declares a version");

    let pkg = std::fs::read_to_string(root.join("npm/package.json")).unwrap();
    let pinned: Vec<(&str, &str)> = pkg
        .lines()
        .filter(|l| l.contains("@youdie006/swapdex-"))
        .filter_map(|l| {
            let mut q = l.split('"').filter(|p| !p.trim().is_empty() && *p != ": ");
            let name = q.next()?;
            let ver = l.rsplit('"').nth(1)?;
            Some((name, ver))
        })
        .collect();

    assert!(!pinned.is_empty(), "no platform packages found to check");
    for (name, ver) in pinned {
        assert_eq!(
            ver, cargo,
            "{name} is pinned at {ver} while this release is {cargo} - an npm \
             install from this tag fetches the wrong binary"
        );
    }
}

/// The GitHub release must say what changed.
///
/// The release page carried the same fixed install blurb on every version while
/// CHANGELOG.md held the actual notes, so a reader on GitHub could not tell one
/// release from the next - and sessionwiki, built the same way, published its
/// releases with an empty body outright. The workflow reads the version's
/// CHANGELOG section now, which makes a MISSING section the new way to ship a
/// blank page. This is the check that catches that on the tag.
#[test]
fn the_changelog_has_notes_for_the_version_cargo_declares() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let toml = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let version = toml
        .lines()
        .find_map(|l| l.strip_prefix("version = \""))
        .and_then(|l| l.split('"').next())
        .expect("Cargo.toml declares a version");

    let out = std::process::Command::new("sh")
        .arg(root.join("scripts/release-notes.sh"))
        .arg(version)
        .arg(root.join("CHANGELOG.md"))
        .output()
        .expect("run scripts/release-notes.sh");
    assert!(
        out.status.success(),
        "release-notes.sh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let notes = String::from_utf8_lossy(&out.stdout);
    assert!(
        !notes.trim().is_empty(),
        "CHANGELOG.md has no section for {version}, so the release would be a blank page"
    );
    // A heading with nothing under it is the same blank page with extra steps.
    assert!(
        notes.lines().filter(|l| !l.trim().is_empty()).count() >= 2,
        "the section for {version} has no content: {notes:?}"
    );
}

/// The extractor must not answer for a version it was not asked about.
///
/// A prefix match would hand 0.15's notes to 0.150.0 and every release after it
/// would carry the wrong text - worse than a blank page, because it reads as
/// deliberate.
#[test]
fn release_notes_match_one_version_exactly() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let run = |v: &str| {
        let out = std::process::Command::new("sh")
            .arg(root.join("scripts/release-notes.sh"))
            .arg(v)
            .arg(root.join("CHANGELOG.md"))
            .output()
            .expect("run scripts/release-notes.sh");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert!(
        run("0.15").trim().is_empty(),
        "a prefix of a real version must match nothing"
    );
    assert!(
        run("9.999.0").trim().is_empty(),
        "a version with no section must match nothing"
    );
    // A `v` prefix is what the tag carries, and it names the same release.
    let toml = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let version = toml
        .lines()
        .find_map(|l| l.strip_prefix("version = \""))
        .and_then(|l| l.split('"').next())
        .unwrap();
    assert_eq!(
        run(version),
        run(&format!("v{version}")),
        "the tag name and the bare version name the same section"
    );
}

/// The placeholder mail domains this project invents for its fixtures. None of
/// them is a mail provider, which is the whole point of the list.
const PLACEHOLDER_DOMAINS: &[&str] = &[
    "example.com",
    "x.com",
    "x.co",
    "y.com",
    "b.com",
    "g.com",
    "company.com",
    "work.com",
    "personal.com",
    "personal.dev",
];

/// A published file must not name a real mailbox.
///
/// Release notes and display fixtures were written by pasting a real machine's
/// output, and two people's addresses came with it - into CHANGELOG.md, which
/// the release workflow publishes as the release page body, and into src/,
/// which crates.io packages verbatim. `exclude` in Cargo.toml drops docs/ and
/// npm/ from the tarball; it drops neither of those.
///
/// A test cannot tell a real address from an invented one, so it checks the one
/// thing that separates them here: an invented address lives on an invented
/// domain. A paste carries a provider's domain and fails.
#[test]
fn no_tracked_file_names_a_real_mailbox() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .expect("run git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    let listing = String::from_utf8(out.stdout).expect("git ls-files prints utf-8 paths");
    let files: Vec<&str> = listing.split('\0').filter(|p| !p.is_empty()).collect();
    assert!(!files.is_empty(), "no tracked files found to check");

    let mut found: Vec<String> = Vec::new();
    for rel in files {
        let Ok(bytes) = std::fs::read(root.join(rel)) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            for domain in addresses(line) {
                if !PLACEHOLDER_DOMAINS.contains(&domain.as_str()) {
                    // The domain, never the address: this assertion's output is
                    // a public CI log, and printing the mailbox would publish
                    // the thing the test exists to keep unpublished.
                    found.push(format!("{rel}:{} (on {domain})", n + 1));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "these lines name a mailbox outside the placeholder domains:\n{}",
        found.join("\n")
    );
}

/// The domain of every address-shaped run in one line.
fn addresses(line: &str) -> Vec<String> {
    let b: Vec<char> = line.chars().collect();
    let local = |c: char| c.is_ascii_alphanumeric() || "._%+-".contains(c);
    let host = |c: char| c.is_ascii_alphanumeric() || c == '.' || c == '-';
    let mut out = Vec::new();
    for (i, c) in b.iter().enumerate() {
        if *c != '@' || i == 0 || !local(b[i - 1]) {
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && host(b[j]) {
            j += 1;
        }
        let domain: String = b[i + 1..j].iter().collect();
        let domain = domain.trim_end_matches('.').to_string();
        // A domain is a dotted name whose last label is alphabetic; that is what
        // separates `a@example.com` from `swapdex@0.155.0`.
        match domain.rsplit_once('.') {
            Some((_, tld)) if tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()) => {
                out.push(domain)
            }
            _ => {}
        }
    }
    out
}

/// The scanner reads mailboxes, not version pins.
///
/// Tracked files are full of `@` that is not mail - `swapdex@0.155.0`,
/// `actions/checkout@v4`, the npm scope in `@youdie006/swapdex-linux-x64`. A
/// scanner that called those addresses would report every release note as a
/// leak and get switched off, so the dotted-name-with-an-alphabetic-last-label
/// rule is what keeps it usable. This pins that rule: without it `pkg@1.2.10`
/// reads as a mailbox on the domain `1.2.10`.
#[test]
fn version_pins_and_scoped_packages_are_not_addresses() {
    assert_eq!(addresses("kong (a@example.com) - 5h"), vec!["example.com"]);
    for line in [
        "swapdex@0.155.0",
        "pkg@1.2.10",
        "uses: actions/checkout@v4",
        "npm i @youdie006/swapdex-linux-x64",
        "a @ b",
    ] {
        assert!(
            addresses(line).is_empty(),
            "not a mailbox: {line:?} read as {:?}",
            addresses(line)
        );
    }
}
