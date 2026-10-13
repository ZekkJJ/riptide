// SPDX-FileCopyrightText: Riptide contributors
// SPDX-License-Identifier: GPL-3.0-or-later

const DEFAULTS = { port: 7739, token: "", enabled: true, notify: true };
const $ = (id) => document.getElementById(id);

async function load() {
  const s = await chrome.storage.local.get(DEFAULTS);
  $("token").value = s.token || "";
  $("port").value = s.port;
  $("enabled").checked = !!s.enabled;
  $("notify").checked = !!s.notify;
  await refreshPermission();
}

async function save() {
  await chrome.storage.local.set({
    token: $("token").value.trim(),
    port: Number($("port").value) || DEFAULTS.port,
    enabled: $("enabled").checked,
    notify: $("notify").checked,
  });
}

function setStatus(el, text, ok) {
  el.textContent = text;
  el.className = "status " + (ok === undefined ? "" : ok ? "ok" : "bad");
}

$("test").onclick = async () => {
  await save();
  setStatus($("status"), "Checking…");
  // The service worker holds the token and does the request, so it stays in one place.
  const res = await chrome.runtime.sendMessage({ kind: "status" });
  if (res?.ok) {
    setStatus($("status"), `Connected to Riptide ${res.version || ""}`.trim(), true);
  } else {
    setStatus($("status"), res?.error || "Could not connect", false);
  }
};

$("reveal").onclick = () => {
  const f = $("token");
  const hidden = f.type === "password";
  f.type = hidden ? "text" : "password";
  $("reveal").textContent = hidden ? "Hide token" : "Show token";
};

for (const id of ["enabled", "notify"]) $(id).onchange = save;

async function refreshPermission() {
  const granted = await chrome.permissions.contains({ origins: ["<all_urls>"] });
  setStatus($("permStatus"), granted ? "Allowed" : "Not allowed", granted ? true : undefined);
  $("grant").disabled = granted;
  $("revoke").disabled = !granted;
}

$("grant").onclick = async () => {
  // Must be called from a user gesture, which a click is.
  await chrome.permissions.request({ origins: ["<all_urls>"] });
  await refreshPermission();
};

$("revoke").onclick = async () => {
  await chrome.permissions.remove({ origins: ["<all_urls>"] });
  await refreshPermission();
};

load();
