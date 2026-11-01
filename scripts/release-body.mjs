#!/usr/bin/env node
// Prints the notes a draft release starts with: the README's manual smoke-test
// checklist (everything between the `smoke-checklist:start` and
// `smoke-checklist:end` markers), every item unticked, under a header the
// maintainer fills in before publishing.
//
// Usage (from the repo root):
//   node scripts/release-body.mjs > dist/RELEASE_BODY.md
//
// Fails instead of printing a partial body when a marker is missing, doubled or
// out of order, or the block holds no checklist item. CI runs it on every PR so
// a README edit cannot break the next tag's Release workflow.
import { readFileSync } from "node:fs";

function fail(msg) {
	console.error(`release-body: ${msg}`);
	process.exit(1);
}

const START = "<!-- smoke-checklist:start";
const END = "<!-- smoke-checklist:end -->";

const lines = readFileSync("README.md", "utf8").split("\n");
const starts = lines.flatMap((l, i) => (l.startsWith(START) ? [i] : []));
const ends = lines.flatMap((l, i) => (l.trim() === END ? [i] : []));
if (starts.length !== 1 || ends.length !== 1) {
	fail(
		`README.md needs exactly one start and one end smoke-checklist marker (found ${starts.length} and ${ends.length})`,
	);
}
if (ends[0] <= starts[0]) fail("README.md's smoke-checklist end marker comes before its start marker");

const checklist = lines
	.slice(starts[0] + 1, ends[0])
	.map((l) => l.replace(/^(\s*[-*] )\[[xX]\]/, "$1[ ]"))
	.join("\n")
	.trim();
const items = checklist.split("\n").filter((l) => /^\s*[-*] \[ \] /.test(l)).length;
if (items === 0) fail("README.md's smoke-checklist block holds no `- [ ]` item");

const version = readFileSync("Cargo.toml", "utf8").match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if (!version) fail('could not find `version = "..."` in Cargo.toml');

process.stdout.write(`<!-- Written by the Release workflow. Before publishing: run the checklist on a
device, tick what passed and mark the rest "fail" or "not run" with why, fill in
the smoke log line, and delete this comment. -->
## Smoke test

**Smoke log:** _(link the v${version} smoke log here before publishing)_
**Run on:** _(date · OpenDeck version · provider and version · Stream Deck model · platform)_

${checklist}
`);
