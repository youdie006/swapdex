#!/usr/bin/env node
// Resolve the prebuilt binary from the platform-specific optionalDependency and
// exec it, forwarding argv, stdio, and the exit code. There is NO install
// script: npm installs only the optionalDependency whose `os`/`cpu` match this
// machine (the esbuild / @biomejs distribution pattern), so nothing runs at
// install time and there is no allow-scripts prompt.

const { spawnSync } = require("child_process");

// swapdex is a Unix tool (Linux, WSL, macOS) - it manages 0600 credential files.
const PKGS = {
  "darwin arm64": "@youdie006/swapdex-darwin-arm64",
  "darwin x64": "@youdie006/swapdex-darwin-x64",
  "linux x64": "@youdie006/swapdex-linux-x64",
  "linux arm64": "@youdie006/swapdex-linux-arm64",
};

function binaryPath() {
  const pkg = PKGS[`${process.platform} ${process.arch}`];
  if (!pkg) return null;
  try {
    // The platform package ships the binary at bin/swapdex; require.resolve
    // finds that exact file inside the installed optional dependency.
    return require.resolve(`${pkg}/bin/swapdex`);
  } catch {
    return null;
  }
}

const bin = binaryPath();
if (!bin) {
  console.error(
    `swapdex: no prebuilt binary for ${process.platform} ${process.arch} ` +
      "(swapdex supports Linux, WSL, and macOS on x64/arm64). " +
      "If your platform is supported, the optional dependency failed to install - " +
      "reinstall, or use `cargo install swapdex`."
  );
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(
    `swapdex: failed to run the prebuilt binary (${result.error.message}). ` +
      "Try `cargo install swapdex`."
  );
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
