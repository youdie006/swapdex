#!/usr/bin/env node
// Thin shim: exec the prebuilt binary that install.js placed next to this file,
// forwarding argv, stdio, and the exit code.

const path = require("path");
const { spawnSync } = require("child_process");

const isWin = process.platform === "win32";
const bin = path.join(__dirname, isWin ? "swapdex.exe" : "swapdex");

const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  console.error(
    "swapdex: prebuilt binary not found. Reinstall the package, or use `cargo install swapdex`."
  );
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
