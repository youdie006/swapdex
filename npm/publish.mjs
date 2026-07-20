#!/usr/bin/env node
// Build the platform packages from a release's prebuilt binaries and publish the
// whole set - the main package plus one os/cpu-restricted package per target.
// No install script anywhere (the esbuild / @biomejs pattern): npm installs only
// the platform package matching the machine, so there is no allow-scripts prompt.
//
// Usage:
//   node publish.mjs [<version>] [--dry-run]
//     <version>   defaults to the main package.json version.
//     --dry-run   builds + `npm pack`s into ./build, publishes nothing.
//
// Binaries come from https://github.com/youdie006/swapdex/releases/download/
//   v<version>/swapdex-<target>.tar.gz  (a release must exist for <version>).

import { execFileSync } from "node:child_process";
import {
  mkdirSync,
  writeFileSync,
  chmodSync,
  rmSync,
  readFileSync,
  copyFileSync,
  existsSync,
} from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const mainPkgPath = join(here, "package.json");
const mainPkg = JSON.parse(readFileSync(mainPkgPath, "utf8"));

const args = process.argv.slice(2);
const dryRun = args.includes("--dry-run");
const version = args.find((a) => !a.startsWith("--")) || mainPkg.version;
const SCOPE = "@youdie006";

// swapdex is a Unix tool (Linux, WSL, macOS). musl binaries are static, so the
// one linux binary per arch serves both glibc and musl hosts.
const PLATFORMS = [
  { pkg: "swapdex-darwin-arm64", os: "darwin", cpu: "arm64", target: "aarch64-apple-darwin" },
  { pkg: "swapdex-darwin-x64", os: "darwin", cpu: "x64", target: "x86_64-apple-darwin" },
  { pkg: "swapdex-linux-x64", os: "linux", cpu: "x64", target: "x86_64-unknown-linux-musl" },
  { pkg: "swapdex-linux-arm64", os: "linux", cpu: "arm64", target: "aarch64-unknown-linux-musl" },
];

const sh = (cmd, cmdArgs, opts = {}) =>
  execFileSync(cmd, cmdArgs, { stdio: "inherit", ...opts });

const buildDir = join(here, "build");
rmSync(buildDir, { recursive: true, force: true });
mkdirSync(buildDir, { recursive: true });

// Keep the published main README in sync with the repo root README.
const rootReadme = join(here, "..", "README.md");
if (existsSync(rootReadme)) copyFileSync(rootReadme, join(here, "README.md"));

// 1) Build each platform package: the binary + an os/cpu-restricted manifest.
for (const p of PLATFORMS) {
  const dir = join(buildDir, p.pkg);
  mkdirSync(join(dir, "bin"), { recursive: true });
  const url = `https://github.com/youdie006/swapdex/releases/download/v${version}/swapdex-${p.target}.tar.gz`;
  const tgz = join(buildDir, `${p.target}.tar.gz`);
  sh("curl", ["-fSL", "-o", tgz, url]);
  sh("tar", ["-xzf", tgz, "-C", join(dir, "bin")]);
  const bin = join(dir, "bin", "swapdex");
  if (!existsSync(bin)) throw new Error(`archive for ${p.target} did not contain the binary`);
  chmodSync(bin, 0o755);
  rmSync(tgz);
  const manifest = {
    name: `${SCOPE}/${p.pkg}`,
    version,
    description: `Prebuilt swapdex binary for ${p.os}-${p.cpu}. Installed automatically by ${SCOPE}/swapdex; do not depend on it directly.`,
    license: mainPkg.license,
    repository: mainPkg.repository,
    homepage: mainPkg.homepage,
    os: [p.os],
    cpu: [p.cpu],
    files: ["bin/swapdex"],
  };
  writeFileSync(join(dir, "package.json"), JSON.stringify(manifest, null, 2) + "\n");
}

// 2) Pin the main package's version + optionalDependencies to this version.
mainPkg.version = version;
mainPkg.optionalDependencies = Object.fromEntries(
  PLATFORMS.map((p) => [`${SCOPE}/${p.pkg}`, version])
);
writeFileSync(mainPkgPath, JSON.stringify(mainPkg, null, 2) + "\n");

// 3) Publish platform packages FIRST (so the main's optionalDependencies
//    resolve on the registry), then the main. --dry-run packs instead.
const npmArgs = dryRun
  ? ["pack", "--pack-destination", buildDir]
  : ["publish", "--access", "public"];
for (const p of PLATFORMS) sh("npm", npmArgs, { cwd: join(buildDir, p.pkg) });
sh("npm", npmArgs, { cwd: here });

console.log(
  dryRun
    ? `\nDRY RUN complete: tarballs in ${buildDir}, nothing published.`
    : `\nPublished swapdex ${version}: main + ${PLATFORMS.length} platform packages.`
);
