#!/usr/bin/env node
// Assembles dist/<plugin-uuid>.sdPlugin/ from assets/ plus the release
// binaries for the targets in manifest.json's CodePaths, and optionally zips
// it into <bin-name>.streamDeckPlugin.
//
// Canonical copy: github.com/jfms7s/opendeck-plugin-ci. Every opendeck-*
// plugin repo carries this file byte for byte (scripts/check-drift.sh in that
// repo checks it); change it there, then copy it to the plugins.
//
// Usage:
//   node build.mjs                       package every CodePaths target that has been built
//   node build.mjs --all                 require every CodePaths target (used by the release)
//   node build.mjs <triple>...           package exactly these targets
//   node build.mjs --check-only          only run the consistency checks below
//   options: --zip                       also write ./<bin-name>.streamDeckPlugin
//            --release                   release package: every CodePaths binary must be in the
//                                        bundle; writes dist/<bin-name>.streamDeckPlugin and
//                                        dist/SHA256SUMS (the assets of a GitHub release)
//            --expect-version <v|vX.Y.Z> fail unless Cargo.toml and manifest.json are at this version
//            --tag <vX.Y.Z>              like --expect-version, but the tag must be exactly v<version>
//   aliases: --require-all = --all, --check-version = --check-only
//
// Build the binaries first: cargo build --release --locked --target <triple>
// (a plain `cargo build --release` counts for the host triple).
//
// Names are read from Cargo.toml ([package] name) and manifest.json (the
// plugin UUID is the first action's UUID minus its last segment), so this
// file carries no per-plugin constants.
import { createHash } from "node:crypto";
import { execFileSync, spawnSync } from "node:child_process";
import {
	chmodSync,
	copyFileSync,
	cpSync,
	existsSync,
	mkdirSync,
	readdirSync,
	readFileSync,
	rmSync,
	statSync,
	writeFileSync,
} from "node:fs";
import { join } from "node:path";

function fail(message) {
	console.error(`build.mjs: ${message}`);
	process.exit(1);
}

// --- arguments -------------------------------------------------------------
const args = process.argv.slice(2);
let requireAll = false;
let checkOnly = false;
let zip = false;
let release = false;
let expectVersion = null;
let tag = null;
const requested = [];
for (let i = 0; i < args.length; i++) {
	const arg = args[i];
	if (arg === "--all" || arg === "--require-all") requireAll = true;
	else if (arg === "--check-only" || arg === "--check-version") checkOnly = true;
	else if (arg === "--zip") zip = true;
	else if (arg === "--release") release = true;
	else if (arg === "--expect-version") {
		expectVersion = args[++i] ?? fail("--expect-version needs a value");
		expectVersion = expectVersion.replace(/^v/, "");
	} else if (arg === "--tag") tag = args[++i] ?? fail("--tag needs a value");
	else if (arg.startsWith("-")) fail(`unknown option ${arg}`);
	else requested.push(arg);
}
if (requireAll && requested.length) fail("pass either --all or explicit targets, not both");
if (release && !requested.length) requireAll = true;

// --- metadata and consistency checks --------------------------------------
const cargoToml = readFileSync("Cargo.toml", "utf8");
const packageSection = cargoToml.split(/^\[/m).find((s) => s.startsWith("package]")) ?? "";
const field = (name) => packageSection.match(new RegExp(`^${name}\\s*=\\s*"([^"]+)"`, "m"))?.[1];
const binName = field("name") ?? fail('no `name = "..."` in Cargo.toml [package]');
const cargoVersion = field("version") ?? fail('no `version = "..."` in Cargo.toml [package]');

const manifest = JSON.parse(readFileSync("assets/manifest.json", "utf8"));
if (cargoVersion !== manifest.Version) {
	fail(`version mismatch: Cargo.toml is ${cargoVersion} but assets/manifest.json is ${manifest.Version} - bump them together`);
}
if (expectVersion !== null && expectVersion !== cargoVersion) {
	fail(`version mismatch: expected ${expectVersion} (from the tag) but Cargo.toml and manifest.json are ${cargoVersion}`);
}
if (tag !== null && tag !== `v${cargoVersion}`) {
	fail(`tag ${tag} does not match the crate/manifest version ${cargoVersion} (expected v${cargoVersion})`);
}

// Every action UUID is "<plugin uuid>.<action>"; the bundle directory is
// "<plugin uuid>.sdPlugin".
const actionUuids = (manifest.Actions ?? []).map((a) => a?.UUID).filter(Boolean);
if (actionUuids.length === 0) fail("manifest.json declares no Actions with a UUID");
const pluginUuid = actionUuids[0].split(".").slice(0, -1).join(".");
if (!pluginUuid) fail(`cannot derive the plugin UUID from action UUID ${actionUuids[0]}`);
for (const uuid of actionUuids) {
	if (!uuid.startsWith(`${pluginUuid}.`)) fail(`action ${uuid} is not under plugin ${pluginUuid}`);
}

// Every literal `const UUID: &str = "..."` in src/ that looks like one of this
// project family's UUIDs (it starts with the family prefix, the first two
// segments of the plugin UUID, e.g. `com.jfms7s.`) must be an action the
// manifest declares. That catches a typo in the plugin segment
// (`com.jfms7s.weatherr.airquality`) as well as an action missing from the
// manifest. UUIDs outside the family (a test double's `test.echo`) are
// ignored. Tolerates rustfmt/clippy reformatting (`&str` vs `&'static str`,
// spacing); a UUID computed rather than written as a literal is not seen, and
// the Rust tests that read manifest.json remain the authority.
function rustFiles(dir) {
	if (!existsSync(dir)) return [];
	return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
		const path = join(dir, entry.name);
		if (entry.isDirectory()) return rustFiles(path);
		return entry.name.endsWith(".rs") ? [path] : [];
	});
}
const familyPrefix = `${pluginUuid.split(".").slice(0, 2).join(".").toLowerCase()}.`;
const uuidConst = /const\s+UUID\s*:\s*&\s*(?:'static\s+)?str\s*=\s*"([^"]+)"/g;
for (const file of rustFiles("src")) {
	for (const [, uuid] of readFileSync(file, "utf8").matchAll(uuidConst)) {
		if (!uuid.toLowerCase().startsWith(familyPrefix) || actionUuids.includes(uuid)) continue;
		const where = uuid.startsWith(`${pluginUuid}.`) ? "" : ` (outside the plugin namespace ${pluginUuid}; a typo?)`;
		fail(`action UUID mismatch: ${file} has ${uuid}${where}, which assets/manifest.json does not declare`);
	}
}

const codePaths = manifest.CodePaths ?? fail("manifest.json has no CodePaths");
for (const [triple, file] of Object.entries(codePaths)) {
	if (file !== `${binName}-${triple}`) fail(`CodePaths["${triple}"] is ${file}, expected ${binName}-${triple}`);
}
for (const key of ["CodePathLin", "CodePathMac", "CodePathWin"]) {
	if (manifest[key] && !Object.values(codePaths).includes(manifest[key])) {
		fail(`${key} ${manifest[key]} is not one of the CodePaths binaries`);
	}
}

if (checkOnly) {
	console.log(`build.mjs: ${binName} ${cargoVersion} metadata is consistent`);
	process.exit(0);
}

// --- pick the binaries -------------------------------------------------------
function hostTriple() {
	try {
		return execFileSync("rustc", ["-vV"], { encoding: "utf8" }).match(/^host:\s*(\S+)/m)?.[1];
	} catch {
		return undefined;
	}
}
const host = hostTriple();

// The newest build for a triple: target/<triple>/release, or target/release
// for the host triple. When both exist the newer file wins and is announced,
// so an old cross build never wins silently.
const announced = new Set();
function binaryFor(triple) {
	const candidates = [join("target", triple, "release", binName)];
	if (triple === host) candidates.push(join("target", "release", binName));
	const existing = candidates
		.filter((p) => existsSync(p))
		.sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs);
	if (existing.length > 1 && !announced.has(existing[0])) {
		announced.add(existing[0]);
		console.log(`build.mjs: using ${existing[0]} (newer than ${existing[1]})`);
	}
	return existing[0];
}

let targets;
if (requested.length) {
	for (const t of requested) {
		if (!(t in codePaths)) fail(`${t} is not in manifest.json CodePaths - OpenDeck would never launch it`);
	}
	targets = requested;
} else {
	targets = Object.keys(codePaths);
}
const found = targets.map((t) => [t, binaryFor(t)]);
const missing = found.filter(([, bin]) => !bin).map(([t]) => t);
if ((requireAll || requested.length) && missing.length) {
	fail(`missing release binaries for ${missing.join(", ")} (run: cargo build --release --locked --target <triple>)`);
}
const built = found.filter(([, bin]) => bin);
if (!built.length) fail(`no release binary found for any of ${targets.join(", ")} (run: cargo build --release --locked)`);
for (const t of missing) console.warn(`build.mjs: skipping ${t} (not built)`);

// --- assemble ---------------------------------------------------------------
const bundle = `${pluginUuid}.sdPlugin`;
const archive = `${binName}.streamDeckPlugin`;
const outDir = join("dist", bundle);
rmSync(outDir, { recursive: true, force: true });
// Never leave a package that doesn't match this bundle: zip appends to an
// existing archive, so a stale one has to go before a new one is written.
rmSync(join("dist", archive), { force: true });
rmSync(join("dist", "SHA256SUMS"), { force: true });
if (zip) rmSync(archive, { force: true });
mkdirSync(outDir, { recursive: true });
cpSync("assets/manifest.json", join(outDir, "manifest.json"));
// Not every plugin has layouts or a property inspector - copy what exists.
// Icon sources (assets/icon-src/) are deliberately not shipped.
for (const dir of ["icons", "layouts", "propertyInspector"]) {
	if (existsSync(join("assets", dir))) cpSync(join("assets", dir), join(outDir, dir), { recursive: true });
}
for (const [triple, bin] of built) {
	const dest = join(outDir, codePaths[triple]);
	copyFileSync(bin, dest);
	chmodSync(dest, 0o755);
	console.log(`build.mjs: ${triple} <- ${bin}`);
}

// Every binary the manifest points OpenDeck at should be in the bundle: a hard
// error for a release, a note for a local build.
const wanted = [...new Set([...Object.values(codePaths), ...["CodePathLin", "CodePathMac", "CodePathWin"].map((k) => manifest[k])])].filter(Boolean);
const absent = wanted.filter((file) => !existsSync(join(outDir, file)));
if (absent.length) {
	const message = `bundle lacks manifest code paths: ${absent.join(", ")}`;
	if (release || requireAll) fail(`${message} - build every target in CodePaths`);
	console.warn(`build.mjs: note: ${message} (fine for a local build)`);
}
console.log(`built ${outDir} (${built.map(([t]) => t).join(", ")})`);

// --- package ------------------------------------------------------------------
function zipBundle(into) {
	const result = spawnSync("zip", ["-r", "-X", "-q", into, bundle], { cwd: "dist", stdio: "inherit" });
	if (result.error || result.status !== 0) {
		fail(`zip failed${result.error ? `: ${result.error.message}` : ""} (is \`zip\` installed?)`);
	}
}
if (zip) {
	zipBundle(join("..", archive));
	console.log(`build.mjs: wrote ${archive}`);
}
if (release) {
	zipBundle(archive);
	const digest = createHash("sha256").update(readFileSync(join("dist", archive))).digest("hex");
	writeFileSync(join("dist", "SHA256SUMS"), `${digest}  ${archive}\n`);
	console.log(`build.mjs: packaged dist/${archive} (sha256 ${digest}) and dist/SHA256SUMS`);
}
