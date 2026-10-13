// SPDX-FileCopyrightText: Riptide contributors
// SPDX-License-Identifier: GPL-3.0-or-later

// Validate the browser extension manifest in CI: it must parse, declare Manifest V3, point
// at files that exist, and not quietly acquire broader permissions than the project intends.
import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const DIR = "extension";
const manifest = JSON.parse(readFileSync(join(DIR, "manifest.json"), "utf8"));
const problems = [];

if (manifest.manifest_version !== 3) {
  problems.push(`manifest_version is ${manifest.manifest_version}, expected 3`);
}

// Every referenced file must exist, or the extension fails to load for users.
const referenced = [
  manifest.background?.service_worker,
  manifest.options_ui?.page,
  manifest.action?.default_popup,
  ...Object.values(manifest.icons ?? {}),
  ...Object.values(manifest.action?.default_icon ?? {}),
].filter(Boolean);

for (const file of referenced) {
  if (!existsSync(join(DIR, file))) problems.push(`manifest references a missing file: ${file}`);
}

// The permission set is a deliberate, documented decision (see docs/ANALYSIS.md section 3).
// Broadening it should be a conscious change to this list, not an accident in a feature PR.
const ALLOWED_PERMISSIONS = new Set([
  "downloads",
  "cookies",
  "contextMenus",
  "storage",
  "notifications",
]);
for (const p of manifest.permissions ?? []) {
  if (!ALLOWED_PERMISSIONS.has(p)) {
    problems.push(`unexpected permission "${p}" — if intended, update ALLOWED_PERMISSIONS here`);
  }
}

// Host access is limited to the loopback service. <all_urls> is requested at runtime from the
// options page, as an optional permission, and must not become a mandatory one.
const hosts = manifest.host_permissions ?? [];
const EXPECTED_HOSTS = ["http://127.0.0.1/*"];
if (JSON.stringify(hosts) !== JSON.stringify(EXPECTED_HOSTS)) {
  problems.push(
    `host_permissions is ${JSON.stringify(hosts)}, expected ${JSON.stringify(EXPECTED_HOSTS)}`,
  );
}
if (hosts.includes("<all_urls>")) {
  problems.push("<all_urls> must stay an optional permission, not a required one");
}
if (manifest.content_scripts) {
  problems.push("the extension deliberately injects no content scripts");
}

if (problems.length > 0) {
  console.error("Extension manifest check failed:");
  for (const p of problems) console.error(`  - ${p}`);
  process.exit(1);
}
console.log(`extension/manifest.json OK (${referenced.length} referenced files present)`);
