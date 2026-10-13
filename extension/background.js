/**
 * FreeDM browser integration.
 *
 * Chrome's downloads API is the interception point rather than blocking webRequest, which
 * Manifest V3 removed: `downloads.onCreated` fires as soon as the browser decides a response
 * is a download, and gives the URL, referrer, filename, media type and size. If the download
 * matches the rules the service supplies, it is cancelled and erased from the browser's own
 * list and handed to FreeDM instead.
 *
 * Cookies are attached when the user has granted host access, because a link from a logged-in
 * page usually will not fetch without them. Access is optional and requested from the options
 * page, so the extension is useful without it rather than demanding read access to every site
 * up front.
 */

const DEFAULTS = {
  port: 7739,
  token: "",
  enabled: true,
  minSize: 256 * 1024,
  extensions: [],
  exclusions: [],
  notify: true,
};

let settings = { ...DEFAULTS };
let rules = { extensions: [], exclusions: [], min_size: DEFAULTS.minSize };
let online = false;

const base = () => `http://127.0.0.1:${settings.port}`;

async function loadSettings() {
  const stored = await chrome.storage.local.get(DEFAULTS);
  settings = { ...DEFAULTS, ...stored };
}

async function api(path, options = {}) {
  const res = await fetch(base() + "/api" + path, {
    ...options,
    headers: {
      "Content-Type": "application/json",
      "X-FreeDM-Token": settings.token,
      ...(options.headers || {}),
    },
  });
  if (!res.ok) {
    let detail = res.status + " " + res.statusText;
    try {
      detail = (await res.json()).error || detail;
    } catch {}
    throw new Error(detail);
  }
  return res.status === 204 ? null : res.json().catch(() => null);
}

/** Refresh the capture rules and the connection indicator. */
async function refresh() {
  if (!settings.token) {
    return setOnline(false, "No token set");
  }
  try {
    rules = await api("/capture-rules");
    setOnline(true);
  } catch (e) {
    setOnline(false, e.message);
  }
}

function setOnline(ok, detail) {
  online = ok;
  chrome.action.setBadgeText({ text: ok ? "" : "!" });
  chrome.action.setBadgeBackgroundColor({ color: ok ? "#3ecf8e" : "#f2555a" });
  chrome.action.setTitle({
    title: ok ? "FreeDM: connected" : `FreeDM: not connected${detail ? " — " + detail : ""}`,
  });
}

/** The lowercase extension of a URL or filename, ignoring any query string. */
function extensionOf(s) {
  if (!s) return null;
  const path = String(s).split(/[?#]/)[0];
  const name = path.split(/[/\\]/).pop() || "";
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1) return null;
  const ext = name.slice(dot + 1).toLowerCase();
  return /^[a-z0-9]{1,12}$/.test(ext) ? ext : null;
}

/** Match a host against a glob like `*.example.com`. */
function hostMatches(host, pattern) {
  const rx = new RegExp(
    "^" + pattern.split("*").map((p) => p.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join(".*") + "$",
    "i",
  );
  return rx.test(host);
}

function isExcluded(url) {
  let host;
  try {
    host = new URL(url).hostname;
  } catch {
    return true;
  }
  return (rules.exclusions || []).some((p) => hostMatches(host, p));
}

/**
 * Decide whether to take a download over.
 *
 * Erring towards leaving it alone matters: wrongly intercepting a small or unknown response
 * is more disruptive than missing one, since the user can always right-click and send it
 * explicitly.
 */
function shouldCapture(item) {
  if (!settings.enabled || !online) return false;
  if (!/^https?:/i.test(item.finalUrl || item.url || "")) return false;
  if (isExcluded(item.finalUrl || item.url)) return false;

  // A download the browser has already finished is not worth restarting.
  if (item.state === "complete") return false;

  const size = item.fileSize > 0 ? item.fileSize : item.totalBytes;
  const min = rules.min_size ?? settings.minSize;
  // An unknown size is allowed through: it is usually a stream, which is exactly the case
  // FreeDM handles better than the browser.
  if (size > 0 && size < min) return false;

  const list = rules.extensions || [];
  if (list.length === 0) return true;
  const ext = extensionOf(item.filename) || extensionOf(item.finalUrl || item.url);
  return ext !== null && list.includes(ext);
}

/** Build a Cookie header for a URL, if the user granted access to read it. */
async function cookieHeaderFor(url) {
  try {
    const granted = await chrome.permissions.contains({ origins: ["<all_urls>"] });
    if (!granted) return null;
    const cookies = await chrome.cookies.getAll({ url });
    if (!cookies || cookies.length === 0) return null;
    return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch {
    return null;
  }
}

async function handOff({ url, filename, referer, source }) {
  const body = { url, source: source || "browser" };
  if (filename) body.filename = filename;
  if (referer) body.referer = referer;

  const cookies = await cookieHeaderFor(url);
  if (cookies) body.cookies = cookies;
  body.user_agent = navigator.userAgent;

  const item = await api("/downloads", { method: "POST", body: JSON.stringify(body) });
  if (settings.notify) {
    notify("Sent to FreeDM", item?.filename || url);
  }
  return item;
}

function notify(title, message) {
  try {
    chrome.notifications.create({
      type: "basic",
      iconUrl: "icons/48.png",
      title,
      message: String(message).slice(0, 200),
    });
  } catch {
    // Notifications are optional; never let one break a download.
  }
}

chrome.downloads.onCreated.addListener(async (item) => {
  await loadSettings();
  if (!shouldCapture(item)) return;

  const url = item.finalUrl || item.url;
  const filename = item.filename ? item.filename.split(/[/\\]/).pop() : undefined;

  try {
    await handOff({ url, filename, referer: item.referrer || undefined });
  } catch (e) {
    // FreeDM would not take it, so leave the browser's own download running rather than
    // cancelling it and losing the download altogether.
    notify("FreeDM could not take this download", e.message + " — the browser will continue it");
    setOnline(false, e.message);
    return;
  }

  // Only now that FreeDM has it is it safe to stop the browser's copy.
  try {
    await chrome.downloads.cancel(item.id);
    await chrome.downloads.erase({ id: item.id });
  } catch {
    // Already finished or gone; harmless.
  }
});

const MENUS = [
  { id: "freedm-link", title: "Download with FreeDM", contexts: ["link"] },
  { id: "freedm-media", title: "Download this with FreeDM", contexts: ["image", "video", "audio"] },
  { id: "freedm-page", title: "Open FreeDM", contexts: ["action"] },
];

chrome.runtime.onInstalled.addListener(async () => {
  await chrome.contextMenus.removeAll();
  for (const m of MENUS) chrome.contextMenus.create(m);
  await loadSettings();
  await refresh();
  if (!settings.token) {
    chrome.runtime.openOptionsPage();
  }
});

chrome.runtime.onStartup.addListener(async () => {
  await loadSettings();
  await refresh();
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  await loadSettings();
  if (info.menuItemId === "freedm-page") {
    chrome.tabs.create({ url: base() + "/" });
    return;
  }
  const url = info.linkUrl || info.srcUrl;
  if (!url) return;
  try {
    // An explicit right-click is a deliberate instruction, so the size and extension rules
    // that govern automatic capture are not applied here.
    await handOff({ url, referer: info.pageUrl || tab?.url, source: "browser" });
  } catch (e) {
    notify("FreeDM could not take this download", e.message);
  }
});

chrome.storage.onChanged.addListener(async () => {
  await loadSettings();
  await refresh();
});

// The popup and options page ask the worker to do the talking, so the token stays in one place.
chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  (async () => {
    await loadSettings();
    try {
      switch (msg?.kind) {
        case "status": {
          const ping = await api("/ping");
          setOnline(true);
          return reply({ ok: true, version: ping?.version });
        }
        case "list": {
          const items = await api("/downloads");
          return reply({ ok: true, items });
        }
        case "add": {
          const item = await handOff({ url: msg.url, source: "browser" });
          return reply({ ok: true, item });
        }
        case "refresh": {
          await refresh();
          return reply({ ok: online });
        }
        default:
          return reply({ ok: false, error: "unknown request" });
      }
    } catch (e) {
      setOnline(false, e.message);
      return reply({ ok: false, error: e.message });
    }
  })();
  // Keep the message channel open for the async reply.
  return true;
});

loadSettings().then(refresh);
