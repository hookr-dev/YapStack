#!/usr/bin/env node
// Fails when an npm Tauri package and its Rust crate resolve to different
// major.minor versions. `tauri build` rejects that mismatch, so catch it in CI
// before a release (e.g. a dependabot bump landing on only one side).
// Compares lockfiles: Cargo.lock vs pnpm-lock.yaml.
import { readFileSync } from "node:fs";

const cargo = readFileSync("Cargo.lock", "utf8");
const pnpm = readFileSync("pnpm-lock.yaml", "utf8");

const crateVersions = (name) => [
  ...cargo.matchAll(new RegExp(`^name = "${name}"\\nversion = "([^"]+)"`, "gm")),
].map((m) => m[1]);

const npmVersions = new Set(
  [...pnpm.matchAll(/^ {2}'?(@tauri-apps\/(?:api|plugin-[a-z-]+))@(\d+\.\d+\.\d+)[^:]*'?:/gm)].map(
    (m) => `${m[1]} ${m[2]}`,
  ),
);

const mm = (v) => v.split(".").slice(0, 2).join(".");
let failed = false;
let checked = 0;
for (const entry of npmVersions) {
  const [pkg, version] = entry.split(" ");
  const crate = pkg === "@tauri-apps/api" ? "tauri" : `tauri-${pkg.slice("@tauri-apps/".length)}`;
  const rust = crateVersions(crate);
  if (rust.length === 0) continue; // npm-only package
  checked++;
  if (!rust.some((r) => mm(r) === mm(version))) {
    console.error(`MISMATCH ${pkg}@${version} vs ${crate}@${rust.join(",")}`);
    failed = true;
  }
}
if (checked === 0) {
  console.error("No Tauri npm/Rust pairs found; lockfile format changed?");
  process.exit(1);
}
if (failed) process.exit(1);
console.log(`Tauri npm/Rust versions aligned (${checked} pairs).`);
