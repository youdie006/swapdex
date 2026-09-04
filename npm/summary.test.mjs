// Run with `node --test npm/**/*.test.mjs`.
import assert from "node:assert/strict";
import { test } from "node:test";

import { publishSummary } from "./summary.js";

test("a clean run says so and is ok", () => {
  const s = publishSummary("0.147.0", 4, []);
  assert.equal(s.ok, true);
  assert.match(s.text, /all resolvable/);
});

test("a run with an unresolvable package must not claim it is resolvable", () => {
  // This is the whole point. The old summary printed "(resolvable)"
  // unconditionally, so a release missing one platform package reported
  // success - and the install that followed removed the working platform
  // package and put nothing back.
  const s = publishSummary("0.147.0", 4, ["@youdie006/swapdex-linux-x64@0.147.0"]);
  assert.equal(s.ok, false, "the run failed and must say so");
  assert.doesNotMatch(s.text, /all resolvable/);
  assert.match(s.text, /NOT resolvable/);
  assert.match(s.text, /swapdex-linux-x64@0\.147\.0/, "names the package");
});

test("it names every unresolvable package, not just the first", () => {
  const s = publishSummary("1.0.0", 4, ["a@1.0.0", "b@1.0.0"]);
  assert.match(s.text, /a@1\.0\.0/);
  assert.match(s.text, /b@1\.0\.0/);
});
