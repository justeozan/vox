#!/usr/bin/env node
// Release helper for Vox. No dependencies (Node 20+).
//
//   node scripts/release.mjs check [--tag vX.Y.Z]   versions are consistent (and match the tag)
//   node scripts/release.mjs bump <X.Y.Z> [--date YYYY-MM-DD]
//                                                    sync every version file and turn the
//                                                    "Unreleased" changelog section into X.Y.Z
//   node scripts/release.mjs notes <X.Y.Z>          print that version's changelog section
//
// Process: see "Releasing" in CONTRIBUTING.md.
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const SEMVER = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/;

// Each target reads/writes its version with one anchored regex, so formatting is preserved.
export const TARGETS = [
  { file: "package.json", re: /^(  "version": ")([^"]+)(")/m },
  { file: "package-lock.json", re: /^(  "version": ")([^"]+)(")/m },
  { file: "package-lock.json", re: /^(    "": \{[^}]*?"version": ")([^"]+)(")/ms },
  { file: "src-tauri/tauri.conf.json", re: /^(  "version": ")([^"]+)(")/m },
  { file: "src-tauri/Cargo.toml", re: /^(version = ")([^"]+)(")/m },
  { file: "src-tauri/Cargo.lock", re: /(\[\[package\]\]\nname = "vox"\nversion = ")([^"]+)(")/ },
];

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (f, r) => readFileSync(join(r, f), "utf8");

export function readVersions(r = root) {
  return TARGETS.map(({ file, re }) => {
    const m = read(file, r).match(re);
    if (!m) throw new Error(`${file}: version not found`);
    return { file, version: m[2] };
  });
}

/** Returns a list of problems (empty when everything is consistent). */
export function checkVersions(r = root, tag) {
  const versions = readVersions(r);
  const expected = versions[0].version;
  const problems = [];
  if (!SEMVER.test(expected)) problems.push(`package.json: "${expected}" is not a semver version`);
  for (const v of versions) {
    if (v.version !== expected) problems.push(`${v.file}: ${v.version} (expected ${expected})`);
  }
  if (tag !== undefined && tag !== `v${expected}`) {
    problems.push(`tag ${tag} does not match the version v${expected}`);
  }
  return problems;
}

/** Rename "## Unreleased" to "## X.Y.Z - date" and open a fresh empty Unreleased section. */
export function releaseChangelog(text, version, date) {
  const m = text.match(/^## Unreleased[ \t]*\n([\s\S]*?)(?=^## |(?![\s\S]))/m);
  if (!m) throw new Error('CHANGELOG.md: no "## Unreleased" section');
  if (!m[1].trim()) throw new Error('CHANGELOG.md: the "Unreleased" section is empty');
  if (new RegExp(`^## ${version.replace(/\./g, "\\.")}( |$)`, "m").test(text)) {
    throw new Error(`CHANGELOG.md: ${version} already has a section`);
  }
  return text.replace(m[0], `## Unreleased\n\n## ${version} - ${date}\n${m[1]}`);
}

/** Body of the "## X.Y.Z" section, without its heading. */
export function extractNotes(text, version) {
  const esc = version.replace(/\./g, "\\.");
  const m = text.match(new RegExp(`^## ${esc}( .*)?\\n([\\s\\S]*?)(?=^## |(?![\\s\\S]))`, "m"));
  if (!m || !m[2].trim()) throw new Error(`CHANGELOG.md: no notes for ${version}`);
  return m[2].trim() + "\n";
}

export function bump(version, date, r = root) {
  if (!SEMVER.test(version)) throw new Error(`"${version}" is not a semver version (X.Y.Z)`);
  const changelog = releaseChangelog(read("CHANGELOG.md", r), version, date);
  const files = new Map();
  for (const { file, re } of TARGETS) {
    const text = files.get(file) ?? read(file, r);
    if (!re.test(text)) throw new Error(`${file}: version not found`);
    files.set(file, text.replace(re, `$1${version}$3`));
  }
  for (const [file, text] of files) writeFileSync(join(r, file), text);
  writeFileSync(join(r, "CHANGELOG.md"), changelog);
}

function main(argv) {
  const [cmd, ...rest] = argv;
  const opt = (name) => {
    const i = rest.indexOf(name);
    return i >= 0 ? rest[i + 1] : undefined;
  };
  if (cmd === "check") {
    const problems = checkVersions(root, opt("--tag"));
    if (problems.length) throw new Error(problems.join("\n"));
    console.log(`Versions consistent: ${readVersions()[0].version}`);
  } else if (cmd === "bump") {
    const date = opt("--date") ?? new Date().toISOString().slice(0, 10);
    bump(rest[0], date);
    console.log(`Bumped to ${rest[0]}`);
  } else if (cmd === "notes") {
    process.stdout.write(extractNotes(read("CHANGELOG.md", root), rest[0]));
  } else {
    throw new Error("usage: release.mjs check [--tag vX.Y.Z] | bump <X.Y.Z> [--date D] | notes <X.Y.Z>");
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  try {
    main(process.argv.slice(2));
  } catch (e) {
    console.error(e.message);
    process.exit(1);
  }
}
