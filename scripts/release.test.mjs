import test from "node:test";
import assert from "node:assert/strict";
import { cpSync, mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { TARGETS, bump, checkVersions, extractNotes, readVersions, releaseChangelog } from "./release.mjs";

const repo = join(dirname(fileURLToPath(import.meta.url)), "..");

function sandbox() {
  const dir = mkdtempSync(join(tmpdir(), "vox-release-"));
  for (const f of [...new Set([...TARGETS.map((t) => t.file), "CHANGELOG.md"])]) {
    mkdirSync(dirname(join(dir, f)), { recursive: true });
    cpSync(join(repo, f), join(dir, f));
  }
  return dir;
}

test("the repository versions are consistent", () => {
  assert.deepEqual(checkVersions(repo), []);
});

test("bump syncs every version file and releases the changelog", () => {
  const dir = sandbox();
  writeFileSync(join(dir, "CHANGELOG.md"), "# Changelog\n\n## Unreleased\n\n### Added\n- thing\n\n## 0.1.0\n\nold\n");
  bump("9.8.7", "2030-01-02", dir);
  assert.deepEqual(checkVersions(dir, "v9.8.7"), []);
  assert.ok(readVersions(dir).every((v) => v.version === "9.8.7"));
  const log = readFileSync(join(dir, "CHANGELOG.md"), "utf8");
  assert.match(log, /## Unreleased\n\n## 9\.8\.7 - 2030-01-02\n\n### Added\n- thing/);
  assert.equal(extractNotes(log, "9.8.7"), "### Added\n- thing\n");
  // dependency versions in the lockfile are left alone
  assert.match(readFileSync(join(dir, "package-lock.json"), "utf8"), /"version": "2\.11\.4"/);
});

test("check reports a drifting file and a mismatching tag", () => {
  const dir = sandbox();
  const p = join(dir, "src-tauri/Cargo.toml");
  writeFileSync(p, readFileSync(p, "utf8").replace(/^version = ".*"/m, 'version = "0.0.1"'));
  const problems = checkVersions(dir, "v5.0.0");
  assert.ok(problems.some((x) => x.startsWith("src-tauri/Cargo.toml")));
  assert.ok(problems.some((x) => x.includes("tag v5.0.0")));
});

test("bump refuses an invalid version, an empty or missing Unreleased section", () => {
  const dir = sandbox();
  assert.throws(() => bump("1.2", "2030-01-01", dir), /semver/);
  assert.throws(() => releaseChangelog("## Unreleased\n\n## 0.1.0\nx\n", "0.2.0", "d"), /empty/);
  assert.throws(() => releaseChangelog("## 0.1.0\nx\n", "0.2.0", "d"), /no "## Unreleased"/);
  assert.throws(() => releaseChangelog("## Unreleased\n- a\n## 0.2.0\nx\n", "0.2.0", "d"), /already/);
});

test("extractNotes fails for an unknown version", () => {
  assert.throws(() => extractNotes("## 0.1.0\nx\n", "0.2.0"), /no notes/);
});
