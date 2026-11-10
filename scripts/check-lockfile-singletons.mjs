#!/usr/bin/env node
// Fails when a package that must be loaded once resolves to more than one
// version in pnpm-lock.yaml. Two copies break instanceof/identity checks at
// runtime (ProseMirror throws "multiple versions of prosemirror-model were
// loaded"; React breaks hooks). Grouped dependabot bumps can leave a stale
// copy behind; `pnpm dedupe` collapses it.
import { readFileSync } from "node:fs";

const SINGLETONS = [
  "prosemirror-model",
  "prosemirror-state",
  "prosemirror-transform",
  "prosemirror-view",
  "react",
  "react-dom",
];

const lockfile = process.argv[2] ?? "pnpm-lock.yaml";
const pnpm = readFileSync(lockfile, "utf8");

const versions = new Map(SINGLETONS.map((name) => [name, new Set()]));
for (const m of pnpm.matchAll(/^ {2}'?((?:@[^/@\s]+\/)?[^@\s']+)@(\d[^(:'\s]*)/gm)) {
  versions.get(m[1])?.add(m[2]);
}

let failed = false;
let checked = 0;
for (const [name, found] of versions) {
  if (found.size === 0) continue; // not a dependency (yet)
  checked++;
  if (found.size > 1) {
    console.error(`DUPLICATE ${name}@${[...found].join(", ")}`);
    failed = true;
  }
}
if (checked === 0) {
  console.error(`No singleton packages found in ${lockfile}; lockfile format changed?`);
  process.exit(1);
}
if (failed) {
  console.error("Run `pnpm dedupe` and commit the updated pnpm-lock.yaml.");
  process.exit(1);
}
console.log(`Lockfile singletons resolve to one version each (${checked} packages).`);
