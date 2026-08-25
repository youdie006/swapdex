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
