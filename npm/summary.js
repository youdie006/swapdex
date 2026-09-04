// What a publish run is allowed to claim at the end.
//
// The check for "can a reader resolve this yet" already existed and printed a
// WARNING when it gave up - and then the summary line said
// "(resolvable)" anyway, unconditionally. A release where one platform package
// had not landed therefore reported success, and the next `npm i -g` removed
// the working platform package and installed nothing in its place, leaving the
// machine with a launcher and no binary.
function publishSummary(version, total, unresolved) {
  if (unresolved.length === 0) {
    return {
      text: `\nPublished swapdex ${version}: main + ${total} platform packages, all resolvable.`,
      ok: true,
    };
  }
  return {
    text:
      `\nPublished swapdex ${version}, but ${unresolved.length} of ${total + 1} package(s) ` +
      `are NOT resolvable yet:\n` +
      unresolved.map((s) => `  ${s}`).join("\n") +
      `\nInstalling this version now can remove a working platform package and ` +
      `install nothing in its place. Wait for the registry and re-check with ` +
      `\`npm view <spec> version\` before installing.`,
    ok: false,
  };
}

// Every package spec the resolvability check must ask npm about.
//
// Scoped, all of them. Publishing built the platform names as
// `${SCOPE}/${p.pkg}` in two places and the check used a bare `p.pkg` in the
// third, so it asked npm about a package that does not exist - it could only
// time out, on every release, for all four platforms.
function wantedSpecs(scope, platforms, version) {
  return [
    `${scope}/swapdex@${version}`,
    ...platforms.map((p) => `${scope}/${p.pkg}@${version}`),
  ];
}

module.exports = { publishSummary, wantedSpecs };
