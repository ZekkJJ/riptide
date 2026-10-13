// SPDX-FileCopyrightText: Riptide contributors
// SPDX-License-Identifier: GPL-3.0-or-later

const $ = (id) => document.getElementById(id);

const fmt = (n) => {
  if (n === null || n === undefined) return "?";
  const u = ["B", "KiB", "MiB", "GiB"];
  let v = n, i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return (i === 0 ? v : v.toFixed(1)) + " " + u[i];
};

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

async function render() {
  const res = await chrome.runtime.sendMessage({ kind: "list" });
  if (!res?.ok) {
    $("dot").className = "dot bad";
    $("msg").textContent = res?.error || "not connected";
    $("list").innerHTML =
      `<div class="none">Start <code>riptided</code> and set the token in Options.</div>`;
    return;
  }
  $("dot").className = "dot ok";

  // Live downloads first, then the most recent few finished ones.
  const live = res.items.filter((i) => ["running", "starting", "queued", "paused"].includes(i.status));
  const rest = res.items.filter((i) => !live.includes(i)).slice(-3);
  const show = [...live, ...rest];

  const speed = live.reduce((a, i) => a + (i.progress?.speed || 0), 0);
  $("msg").textContent = live.length ? `${live.length} active · ${fmt(speed)}/s` : "idle";

  $("list").innerHTML = show.length
    ? show.map((i) => {
        const p = i.progress || {};
        const pct = p.total ? Math.min(100, (p.downloaded / p.total) * 100) : 0;
        return `<div class="item">
          <div class="n" title="${esc(i.filename)}">${esc(i.filename)}</div>
          <div class="bar"><i style="width:${pct}%"></i></div>
          <div class="sub">${i.status} · ${fmt(p.downloaded || 0)} / ${fmt(p.total)}${
            p.speed ? " · " + fmt(p.speed) + "/s" : ""
          }</div>
        </div>`;
      }).join("")
    : `<div class="none">No downloads.</div>`;
}

$("open").onclick = async () => {
  const { port = 7739 } = await chrome.storage.local.get({ port: 7739 });
  chrome.tabs.create({ url: `http://127.0.0.1:${port}/` });
};
$("opts").onclick = () => chrome.runtime.openOptionsPage();

render();
setInterval(render, 1000);
