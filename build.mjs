#!/usr/bin/env node
// Assembles dist/<uuid>.sdPlugin/ from assets/ plus the release binary of each
// given target, and with --release also zips it into the installable
// dist/<bin>.streamDeckPlugin and writes dist/SHA256SUMS.
//
// Usage: node build.mjs [--release] [--tag <git-tag>] [<target-triple>...]
//   <target-triple>  each needs `cargo build --release --locked --target <triple>` first;
//                    with none, packages the host build (target/release/<bin>)
//   --release        every CodePaths target in the manifest must be present;
//                    produce the zip and its checksum file
//   --tag <git-tag>  fail unless the tag is v<version>
//
// The plugin UUID and binary name are derived from assets/manifest.json and
// Cargo.toml, so nothing here is specific to one plugin.
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { copyFileSync, cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

function fail(message) {
	console.error(`build.mjs: ${message}`);
	process.exit(1);
}

// --- arguments -------------------------------------------------------------
const args = process.argv.slice(2);
let release = false;
let tag = null;
const targets = [];
for (let i = 0; i < args.length; i++) {
	if (args[i] === "--release") release = true;
	else if (args[i] === "--tag") tag = args[++i] ?? fail("--tag needs a value");
	else if (args[i].startsWith("--")) fail(`unknown option ${args[i]}`);
	else targets.push(args[i]);
}

// --- identity and versions -------------------------------------------------
const cargoToml = readFileSync("Cargo.toml", "utf8");
const packageSection = cargoToml.split(/^\[/m).find((s) => s.startsWith("package]")) ?? "";
const field = (name) => packageSection.match(new RegExp(`^${name}\\s*=\\s*"([^"]+)"`, "m"))?.[1];
const binName = field("name") ?? fail("no [package] name in Cargo.toml");
const cargoVersion = field("version") ?? fail("no [package] version in Cargo.toml");

const manifest = JSON.parse(readFileSync("assets/manifest.json", "utf8"));
// Action UUIDs are "<plugin uuid>.<action>"; the bundle directory is "<plugin uuid>.sdPlugin".
const actionUuids = (manifest.Actions ?? []).map((a) => a.UUID);
if (actionUuids.length === 0) fail("assets/manifest.json declares no Actions");
const uuid = actionUuids[0].slice(0, actionUuids[0].lastIndexOf("."));
for (const actionUuid of actionUuids) {
	if (!actionUuid.startsWith(`${uuid}.`)) fail(`action ${actionUuid} is not under plugin ${uuid}`);
}

// Cargo.toml's version and manifest.json's "Version" have nothing keeping them
// in sync (a unit test checks this too) - catch drift before shipping.
if (cargoVersion !== manifest.Version) {
	fail(`version mismatch: Cargo.toml is ${cargoVersion} but assets/manifest.json is ${manifest.Version} - bump them together`);
}
if (tag !== null && tag !== `v${cargoVersion}`) {
	fail(`tag ${tag} does not match the crate/manifest version ${cargoVersion} (expected v${cargoVersion})`);
}

// --- binaries --------------------------------------------------------------
function hostTriple() {
	const info = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
	return info.match(/^host:\s*(\S+)/m)?.[1] ?? fail("could not read the host triple from `rustc -vV`");
}

const binaries = new Map(); // triple -> path of its release binary
if (targets.length === 0) {
	const triple = hostTriple();
	const candidates = [join("target", triple, "release", binName), join("target", "release", binName)];
	const found = candidates.find(existsSync) ?? fail(`missing release binary (run: cargo build --release --locked)`);
	binaries.set(triple, found);
} else {
	for (const triple of targets) {
		const path = join("target", triple, "release", binName);
		if (!existsSync(path)) fail(`missing release binary: ${path} (run: cargo build --release --locked --target ${triple})`);
		binaries.set(triple, path);
	}
}

// --- bundle ----------------------------------------------------------------
const bundleName = `${uuid}.sdPlugin`;
const outDir = join("dist", bundleName);
rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });

cpSync("assets/manifest.json", join(outDir, "manifest.json"));
// Not every plugin has layouts or a property inspector - copy what exists.
for (const dir of ["icons", "layouts", "propertyInspector"]) {
	if (existsSync(join("assets", dir))) {
		cpSync(join("assets", dir), join(outDir, dir), { recursive: true });
	}
}
for (const [triple, path] of binaries) {
	copyFileSync(path, join(outDir, `${binName}-${triple}`));
}

// Every binary the manifest points OpenDeck at must be in the bundle.
const codePaths = [...Object.values(manifest.CodePaths ?? {}), manifest.CodePathLin].filter(Boolean);
const missing = [...new Set(codePaths)].filter((p) => !existsSync(join(outDir, p)));
if (missing.length > 0) {
	const message = `bundle lacks manifest code paths: ${missing.join(", ")}`;
	if (release) fail(`${message} - build every target in CodePaths`);
	console.warn(`build.mjs: warning: ${message} (fine for a local build)`);
}
console.log(`built ${outDir} for ${[...binaries.keys()].join(", ")}`);

// --- release package -------------------------------------------------------
if (release) {
	const zipName = `${binName}.streamDeckPlugin`;
	rmSync(join("dist", zipName), { force: true }); // zip appends to an existing archive
	execFileSync("zip", ["-r", "-X", "-q", zipName, bundleName], { cwd: "dist", stdio: "inherit" });
	const digest = createHash("sha256").update(readFileSync(join("dist", zipName))).digest("hex");
	writeFileSync(join("dist", "SHA256SUMS"), `${digest}  ${zipName}\n`);
	console.log(`packaged dist/${zipName} (sha256 ${digest}) and dist/SHA256SUMS`);
}
