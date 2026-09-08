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
