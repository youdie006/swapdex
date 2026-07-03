#!/usr/bin/env node
// Postinstall: download the prebuilt swapdex binary for this platform from the
// matching GitHub release and place it next to the JS shim. Never fails the npm
// install hard - on any problem it prints a hint to use `cargo install swapdex`
// and exits 0, so the package install does not break.

const fs = require("fs");
const os = require("os");
const path = require("path");
const https = require("https");
const { execFileSync } = require("child_process");

const version = require("./package.json").version;

const TARGETS = {
  "darwin arm64": "aarch64-apple-darwin",
  "darwin x64": "x86_64-apple-darwin",
  "linux x64": "x86_64-unknown-linux-musl",
  "linux arm64": "aarch64-unknown-linux-musl",
  "win32 x64": "x86_64-pc-windows-msvc",
};

const isWin = process.platform === "win32";
const key = `${process.platform} ${process.arch}`;
const target = TARGETS[key];
const binDir = path.join(__dirname, "bin");
const binName = isWin ? "swapdex.exe" : "swapdex";

function bail(msg) {
  console.error(`swapdex: ${msg} Try \`cargo install swapdex\` instead.`);
  process.exit(0); // do not break the whole npm install
}

if (!target) bail(`no prebuilt binary for ${key}.`);

const ext = isWin ? "zip" : "tar.gz";
const url = `https://github.com/youdie006/swapdex/releases/download/v${version}/swapdex-${target}.${ext}`;

function download(u, dest, cb, redirects = 0) {
  https
    .get(u, { headers: { "User-Agent": "swapdex-npm" } }, (res) => {
      if (
        [301, 302, 307, 308].includes(res.statusCode) &&
        res.headers.location &&
        redirects < 5
      ) {
        res.resume();
        return download(res.headers.location, dest, cb, redirects + 1);
      }
      if (res.statusCode !== 200) {
        return cb(new Error(`HTTP ${res.statusCode}`));
      }
      const f = fs.createWriteStream(dest);
      res.pipe(f);
      f.on("finish", () => f.close(() => cb(null)));
      f.on("error", cb);
    })
    .on("error", cb);
}

fs.mkdirSync(binDir, { recursive: true });
const archive = path.join(os.tmpdir(), `swapdex-${target}.${ext}`);

download(url, archive, (err) => {
  if (err) bail(`could not download the prebuilt binary (${err.message}).`);
  try {
    // bsdtar (bundled on modern macOS, Linux, and Windows 10+) extracts both
    // .tar.gz and .zip.
    execFileSync("tar", ["-xf", archive, "-C", binDir], { stdio: "ignore" });
    const bin = path.join(binDir, binName);
    if (!fs.existsSync(bin)) bail("the downloaded archive did not contain the binary.");
    if (!isWin) fs.chmodSync(bin, 0o755);
    fs.unlinkSync(archive);
    console.log(`swapdex: installed prebuilt binary (${target}).`);
  } catch (e) {
    bail(`failed to extract the binary (${e.message}).`);
  }
});
