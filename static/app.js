"use strict";
// cmux web remote: a Claude-app-like client for cmux. The drawer lists cmux
// workspaces; each holds its agent sessions, terminals, browsers and documents.
// The main column shows one of them at a time with a view made for its kind.

const $ = (id) => document.getElementById(id);
const PHONE = matchMedia("(max-width: 900px)");
const saved = (key, fallback) => { try { return JSON.parse(localStorage.getItem(key)) ?? fallback; } catch (e) { return fallback; } };
const save = (key, value) => localStorage.setItem(key, JSON.stringify(value));
// Browser panels open in reader mode unless switched to the screenshot; these are the switched ones.
const shotView = new Set(saved("browserShot", []));
const readerOn = { has: (id) => !shotView.has(id) };

const state = {
  tree: null,
  notifications: [],
  sel: saved("cmuxSel", null),          // {wsId, surfaceId} or {wsId, view: "new"}
  expanded: new Set(saved("cmuxExpanded", [])),
  viewBySurface: saved("cmuxViews", {}), // agent surfaces: "chat" | "terminal"
  mounted: "",                          // key of what #view shows
  screens: {},
  fromBottom: {},
  fontMode: saved("cmuxFontMode", "fit"),
  chats: {},
  chatPolls: {},
  poll: null,
  browser: null,
  moreDetails: new Set(),  // workspaces whose status entries are all shown
  openNotes: new Set(),    // workspaces whose notification is shown up to the Mac's line limit
  secrets: [],             // open `psycommux secret` requests
  secretOpen: "",          // the one whose form is showing
  pushOn: false,           // this device gets Web Push; the page shows no notifications itself
  openAfterLoad: null,     // a tapped notification's URL, opened once the tree is in
  file: null,
  pendingNew: null,                     // {wsId, until}
  newCwd: null,                         // the folder input on the New screen
  wsFilter: saved("cmuxWsFilter", "all"), // "all" | "needs"
};

async function api(path, body, signal) {
  const opts = body === undefined ? { signal } : { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body), signal };
  const res = await fetch(path, opts);
  if (res.status === 401) { location.href = "/"; throw new Error("unauthorized"); }
  const data = await res.json();
  if (!res.ok) throw Object.assign(new Error(data.message || data.error || res.statusText), { code: data.error });
  return data;
}

function el(tag, props, ...children) {
  const node = document.createElement(tag);
  Object.assign(node, props || {});
  for (const c of children) if (c != null) node.append(c);
  return node;
}
const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));

// cmux renders Markdown in descriptions and strips it from notification previews.
// One readable line: drop XML-ish wrappers agents put in prompts (<task-notification>, <pasted_content>).
function oneLine(text) {
  return plainMarkdown(String(text || "").replace(/<\/?[a-zA-Z_][\w-]*(\s[^>]*)?>/g, " ")).replace(/\s+/g, " ").trim();
}
function plainMarkdown(text) {
  return String(text || "").replace(/\x1b\[[0-9;?]*[ -\/]*[@-~]/g, "").replace(/\[([^\]]+)\]\([^)]+\)/g, "$1").replace(/(\*\*|__|`)/g, "");
}

// ---- model ----------------------------------------------------------------------
function allWorkspaces() {
  return ((state.tree && state.tree.windows) || []).flatMap((w) => w.workspaces.map((ws) => Object.assign(ws, { windowId: w.id })));
}
function findWorkspace(id) { return allWorkspaces().find((w) => w.id === id) || null; }
function findSurface(surfaceId) {
  for (const ws of allWorkspaces()) for (const pane of ws.panes) for (const s of pane.surfaces)
    if (s.id === surfaceId) return { ws, pane, surface: s };
  return null;
}
function isAgent(s) { return !!(s && s.agent); }
const KIND_RANK = (s) => (isAgent(s) ? 0 : s.type === "terminal" ? 1 : s.type === "browser" ? 2 : 3);
function wsItems(ws) {
  return ws.panes.flatMap((p) => p.surfaces).map((s, i) => ({ s, i }))
    .sort((a, b) => KIND_RANK(a.s) - KIND_RANK(b.s) || a.i - b.i).map((x) => x.s);
}
function cleanTitle(t) { return String(t || "").replace(/^[\s✳✻✽✶✢·⏺●◐◓◑◒⠁-⣿]+/u, "").trim(); }
function itemLabel(s) {
  if (s.type === "browser") {
    const title = cleanTitle(s.title);
    if (title) return title;
    try { return new URL(s.url).host; } catch (e) { return s.url || "Browser"; }
  }
  return cleanTitle(s.title) || s.ref;
}
function itemIcon(s) {
  if (isAgent(s)) return { t: "✳", c: "var(--acc)" };
  return { terminal: { t: "›_" }, browser: { t: "◍" }, markdown: { t: "¶" }, filepreview: { t: "▤" } }[s.type] || { t: "•" };
}
function agentState(s) { return s && s.agent ? s.agent.state : ""; }
function viewFor(s) {
  if (!s) return "none";
  if (s.type === "terminal") return s.agent && s.agent.chat && (state.viewBySurface[s.id] || "chat") === "chat" ? "chat" : "terminal";
  if (s.type === "browser") return "browser";
  if (s.type === "markdown" || s.type === "filepreview") return "file";
  return "unsupported";
}
function unreadFor(ws) {
  const listed = state.notifications.filter((n) => !n.is_read && n.workspace_id === ws.id).length;
  return Math.max(ws.unread_count || 0, listed);
}
function wsAttention(ws) {
  const items = wsItems(ws);
  // Live agent sessions are authoritative; sidebar status pills can be stale.
  if (items.some(isAgent)) {
    return { needsInput: items.some((s) => agentState(s) === "needs_input"), working: items.some((s) => agentState(s) === "working") };
  }
  const work = ((ws.sidebar && ws.sidebar.statuses) || []).map((x) => x.work);
  return {
    needsInput: items.some((s) => agentState(s) === "needs_input") || work.includes("waiting"),
    working: items.some((s) => agentState(s) === "working") || work.includes("running") || work.includes("subagents"),
  };
}
function selected() {
  const sel = state.sel;
  if (!sel) return { ws: null, surface: null };
  const ws = findWorkspace(sel.wsId);
  const hit = sel.surfaceId && findSurface(sel.surfaceId);
  return { ws: hit ? hit.ws : ws, surface: hit ? hit.surface : null };
}

// ---- data -----------------------------------------------------------------------
async function loadTree() {
  try {
    state.tree = await api("/api/tree");
  } catch (e) {
    $("wslist").replaceChildren(el("div", { className: "empty", textContent: "Error: " + e.message }));
    return;
  }
  if (state.secrets.length && !state.secretOpen) renderSecrets();  // workspace names arrived
  if (state.openAfterLoad) { const url = state.openAfterLoad; state.openAfterLoad = null; openFromUrl(url); }
  if (state.pendingNew) {
    const ws = findWorkspace(state.pendingNew.wsId);
    const agent = ws && wsItems(ws).find(isAgent);
    if (agent) { state.pendingNew = null; select(ws.id, agent.id); return; }
    if (Date.now() > state.pendingNew.until) state.pendingNew = null;
  }
  const { ws, surface } = selected();
  if (!state.sel || !ws) {
    const onMac = allWorkspaces().find((w) => w.selected) || allWorkspaces()[0];
    if (onMac) { openWorkspace(onMac.id); return; }
  } else if (state.sel.surfaceId && !surface) {
    openWorkspace(ws.id);  // the surface closed
    return;
  }
  render();
}
let treeTimer = null;
function scheduleTree(ms = 400) { clearTimeout(treeTimer); treeTimer = setTimeout(loadTree, ms); }
setInterval(() => { if (!document.hidden) loadTree(); }, 10000);  // agent states change without events

// ---- navigation -----------------------------------------------------------------
function select(wsId, surfaceId, view) {
  state.sel = view ? { wsId, view } : { wsId, surfaceId };
  save("cmuxSel", state.sel);
  // On a phone only the open workspace stays unfolded, so the list stays short.
  if (wsId && PHONE.matches) { state.expanded = new Set([wsId]); save("cmuxExpanded", [wsId]); }
  else if (wsId && !state.expanded.has(wsId)) { state.expanded.add(wsId); save("cmuxExpanded", [...state.expanded]); }
  document.body.classList.remove("show-nav");
  render();
  markRead();
}
function openWorkspace(wsId) {
  const ws = findWorkspace(wsId);
  const first = ws && wsItems(ws)[0];
  select(wsId, first ? first.id : null);
}

function render() {
  renderNav();
  renderMain();
}

function renderNav() {
  const list = $("wslist");
  const scroll = list.scrollTop;
  const needle = $("filter").value.trim().toLowerCase();
  list.replaceChildren();
  const sel = state.sel || {};
  const all = allWorkspaces();
  const needs = all.filter(needsYou);
  for (const b of $("wsfilter").children) {
    b.classList.toggle("on", b.dataset.f === state.wsFilter);
    if (b.dataset.f === "needs") b.textContent = needs.length ? `Needs you · ${needs.length}` : "Needs you";
  }
  for (const ws of state.wsFilter === "needs" && !needle ? needs : all) {
    const items = wsItems(ws);
    const hay = [ws.title, ws.description, ws.latest_notification_text, ws.directory, ws.branch, ...items.map(itemLabel)].join(" ").toLowerCase();
    if (needle && !hay.includes(needle)) continue;
    const open = !!needle || state.expanded.has(ws.id);
    const attn = wsAttention(ws);
    const unread = unreadFor(ws);
    const opt = sidebarOptions();
    // On a phone a closed row is two lines: the name and what matters most. Opening it
    // shows everything the Mac's sidebar shows, and the panels.
    const compact = PHONE.matches && !open;
    const more = el("button", { className: "ws-more", textContent: "⋯", title: "Workspace actions" });
    more.onclick = (e) => { e.stopPropagation(); showMenu(more, workspaceMenu(ws)); };
    const head = el("div", { className: "ws-head" },
      ws.custom_color ? el("span", { className: "rail", style: "background:" + ws.custom_color }) : null,
      el("span", { className: "chev", textContent: "▸" }),
      el("div", { className: "meta" },
        el("div", { className: "name" + (opt.wrapWorkspaceTitles ? " wrap" : "") },
          ws.pinned ? el("span", { className: "pin", textContent: "📌" }) : null, ws.title || ws.ref),
        ...(compact ? wsSummary(ws, attn, opt) : wsDetails(ws, attn, opt))),
      attn.working && !attn.needsInput && opt.showAgentActivity ? el("span", { className: "spin" }) : null,
      unread ? el("span", { className: "badge", textContent: unread > 99 ? "99+" : unread }) : null,
      more);
    head.onclick = () => {
      if (state.expanded.has(ws.id)) state.expanded.delete(ws.id); else state.expanded.add(ws.id);
      save("cmuxExpanded", [...state.expanded]);
      renderNav();
    };
    const box = el("div", { className: "ws" + (open ? " open" : "") + (ws.selected ? " mac" : "") }, head);
    const itemsBox = el("div", { className: "ws-items" });
    for (const s of items) {
      const icon = itemIcon(s);
      const st = agentState(s);
      const unreadItem = state.notifications.some((n) => !n.is_read && n.surface_id === s.id);
      const row = el("div", { className: "item" + (sel.surfaceId === s.id ? " on" : "") },
        el("span", { className: "ic", textContent: icon.t, style: icon.c ? "color:" + icon.c : "" }),
        el("span", { className: "lbl", textContent: itemLabel(s) }),
        st === "working" ? el("span", { className: "st working" })
          : st === "needs_input" ? el("span", { className: "st needs_input", textContent: "input" }) : null,
        unreadItem ? el("span", { className: "dot" }) : null);
      row.onclick = () => select(ws.id, s.id);
      itemsBox.append(row);
    }
    if (!items.length) itemsBox.append(el("div", { className: "item", textContent: "(empty)", style: "color:var(--dim)" }));
    box.append(itemsBox);
    list.append(box);
  }
  if (!list.children.length) list.append(el("div", { className: "empty", textContent: needle ? "No matches." : state.wsFilter === "needs" ? "Nothing needs you." : "No workspaces." }));
  list.scrollTop = scroll;
}

for (const b of $("wsfilter").children) {
  b.onclick = () => { state.wsFilter = b.dataset.f; save("cmuxWsFilter", state.wsFilter); $("wslist").scrollTop = 0; renderNav(); };
}

const GRAY = new Set(["#8E8E93", "#98989D", "#999999", "#808080", "#AEAEB2"]);
// Unread notifications, or an agent waiting for an answer.
function needsYou(ws) { return unreadFor(ws) > 0 || wsAttention(ws).needsInput; }

// One line for a closed row on a phone: an agent waiting, else a colored status
// (a "decide" pill), else the latest notification, the description, a status, the folder.
function wsSummary(ws, attn, opt) {
  if (attn.needsInput) return [el("div", { className: "sub attn", textContent: "Needs your input" })];
  if (opt.hideAllDetails) return [];
  const sb = ws.sidebar || {};
  const statuses = opt.showCustomMetadata ? (sb.statuses || []) : [];
  const status = (st) => {
    const line = el("div", { className: "d-line d-status d-sum" }, statusIcon(st.icon), el("span", { className: "grow", textContent: plainMarkdown(String(st.value || "")) }));
    if (st.color) line.style.color = st.color;
    return [line];
  };
  // Gray pills ("Idle") and plain ones ("claude-code · Done") say little; a colored one
  // ("待你决定", "已推进 → Rework") is someone asking for something.
  const colored = statuses.find((st) => st.color && !GRAY.has(String(st.color).toUpperCase()));
  if (colored) return status(colored);
  const text = (opt.showNotificationMessage && ws.latest_notification_text) || (opt.showWorkspaceDescription && ws.description);
  if (text) return [el("div", { className: "d-note d-sum", textContent: oneLine(plainMarkdown(String(text))) })];
  const where = [ws.branch && "⎇ " + ws.branch, shortPath(ws.directory, true)].filter(Boolean).join(" · ");
  return where ? [el("div", { className: "d-line mono", textContent: where })] : [];
}

// ---- workspace rows: the details cmux's own sidebar shows -------------------------
// In the Mac sidebar's order: description, latest notification, SSH target, status entries and
// Markdown blocks, newest log line, progress, branch and directory, pull request, ports. Each
// follows the Mac's sidebar settings (the server reads them); unset ones use cmux's defaults.
const SIDEBAR_DEFAULTS = {
  hideAllDetails: false, showWorkspaceDescription: true, showNotificationMessage: true, notificationMessageLineLimit: 12,
  showBranchDirectory: true, stackBranchDirectory: false, pathLastSegmentOnly: false, showPullRequests: true, showSSH: true,
  showPorts: true, showLog: true, showProgress: true, showAgentActivity: true, showCustomMetadata: true, wrapWorkspaceTitles: false,
};
function sidebarOptions() {
  return Object.assign({}, SIDEBAR_DEFAULTS, (state.tree && state.tree.sidebar_settings) || {});
}
// Status icons: SF Symbol names cmux sends, drawn with Lucide (ISC, see LICENSE-LUCIDE).
const SF_TO_LUCIDE = {
  "checkmark": "check",
  "checkmark.circle": "circle-check",
  "checkmark.seal": "circle-check",
  "xmark": "x",
  "xmark.circle": "circle-x",
  "xmark.octagon": "circle-x",
  "exclamationmark": "circle-alert",
  "exclamationmark.circle": "circle-alert",
  "exclamationmark.triangle": "triangle-alert",
  "exclamationmark.octagon": "octagon-alert",
  "questionmark": "circle-question-mark",
  "questionmark.circle": "circle-question-mark",
  "info": "info",
  "bolt": "zap",
  "hourglass": "hourglass",
  "clock": "clock",
  "timer": "timer",
  "play": "play",
  "play.circle": "circle-play",
  "pause": "pause",
  "pause.circle": "circle-pause",
  "stop": "square",
  "stop.circle": "circle-stop",
  "bell": "bell",
  "bell.slash": "bell-off",
  "sparkle": "sparkles",
  "sparkles": "sparkles",
  "wand.and.stars": "wand-sparkles",
  "brain": "brain",
  "hammer": "hammer",
  "wrench": "wrench",
  "wrench.and.screwdriver": "wrench",
  "gear": "settings",
  "gearshape": "settings",
  "terminal": "terminal",
  "apple.terminal": "terminal",
  "folder": "folder",
  "doc": "file-text",
  "doc.text": "file-text",
  "photo": "image",
  "display": "monitor",
  "desktopcomputer": "monitor",
  "network": "network",
  "icloud.and.arrow.up": "cloud-upload",
  "icloud": "cloud",
  "cloud": "cloud",
  "leaf": "leaf",
  "ladybug": "bug",
  "ant": "bug",
  "star": "star",
  "flag": "flag",
  "heart": "heart",
  "person": "user",
  "person.2": "users",
  "hand": "hand",
  "hand.raised": "hand",
  "eye": "eye",
  "lock": "lock",
  "key": "key-round",
  "globe": "globe",
  "magnifyingglass": "search",
  "arrow.clockwise": "refresh-cw",
  "arrow.triangle.2.circlepath": "refresh-cw",
  "arrow.2.circlepath": "refresh-cw",
  "arrow.triangle.branch": "git-branch",
  "arrow.triangle.pull": "git-pull-request",
  "arrow.up": "arrow-up",
  "arrow.down": "arrow-down",
  "paperplane": "send",
  "tray": "inbox",
  "trash": "trash-2",
  "pencil": "pencil",
  "link": "link",
  "cpu": "cpu",
  "server.rack": "server",
  "bubble": "message-square",
  "bubble.left": "message-square",
  "text.bubble": "message-square",
  "book": "book-open",
  "list.bullet": "list",
  "chart.bar": "chart-column",
  "gauge": "gauge",
  "plus": "plus",
  "minus": "minus",
  "circle": "circle",
  "largecircle": "circle",
  "square": "square",
  "point.3.filled.connected.trianglepath.dotted": "waypoints",
  "point.3.connected.trianglepath.dotted": "waypoints",
  "rectangle.on.rectangle": "copy",
  "cup.and.saucer": "coffee",
  "moon": "moon",
  "sun.max": "sun",
  "shippingbox": "package",
  "tag": "tag",
  "calendar": "calendar",
  "envelope": "mail",
  "phone": "phone",
  "wifi": "wifi",
};
const LUCIDE = {
  "check": "<path d=\"M20 6 9 17l-5-5\"/>",
  "circle-check": "<circle cx=\"12\" cy=\"12\" r=\"10\"/><path d=\"m16 9-5.5 5.5L8 12\"/>",
  "x": "<path d=\"M18 6 6 18\"/><path d=\"m6 6 12 12\"/>",
  "circle-x": "<circle cx=\"12\" cy=\"12\" r=\"10\"/><path d=\"m15 9-6 6\"/><path d=\"m9 9 6 6\"/>",
  "circle-alert": "<circle cx=\"12\" cy=\"12\" r=\"10\"/><line x1=\"12\" x2=\"12\" y1=\"8\" y2=\"12\"/><line x1=\"12\" x2=\"12.01\" y1=\"16\" y2=\"16\"/>",
  "triangle-alert": "<path d=\"m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3\"/><path d=\"M12 9v4\"/><path d=\"M12 17h.01\"/>",
  "octagon-alert": "<path d=\"M12 16h.01\"/><path d=\"M12 8v4\"/><path d=\"M15.312 2a2 2 0 0 1 1.414.586l4.688 4.688A2 2 0 0 1 22 8.688v6.624a2 2 0 0 1-.586 1.414l-4.688 4.688a2 2 0 0 1-1.414.586H8.688a2 2 0 0 1-1.414-.586l-4.688-4.688A2 2 0 0 1 2 15.312V8.688a2 2 0 0 1 .586-1.414l4.688-4.688A2 2 0 0 1 8.688 2z\"/>",
  "circle-question-mark": "<circle cx=\"12\" cy=\"12\" r=\"10\"/><path d=\"M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3\"/><path d=\"M12 17h.01\"/>",
  "info": "<circle cx=\"12\" cy=\"12\" r=\"10\"/><path d=\"M12 16v-4\"/><path d=\"M12 8h.01\"/>",
  "zap": "<path d=\"M15.914 4a1.5 1.5 0 00-2.474-1.561l-9 9A1.5 1.5 0 005.5 14h4.002a.5.5 0 01.471.666L8.086 20a1.5 1.5 0 002.475 1.56l9-9A1.5 1.5 0 0018.5 10h-3.997a.5.5 0 01-.472-.667z\"/>",
  "hourglass": "<path d=\"M5 22h14\"/><path d=\"M5 2h14\"/><path d=\"M17 22v-4.172a2 2 0 0 0-.586-1.414L12 12l-4.414 4.414A2 2 0 0 0 7 17.828V22\"/><path d=\"M7 2v4.172a2 2 0 0 0 .586 1.414L12 12l4.414-4.414A2 2 0 0 0 17 6.172V2\"/>",
  "clock": "<circle cx=\"12\" cy=\"12\" r=\"10\"/><path d=\"M12 6v6l4 2\"/>",
  "timer": "<line x1=\"10\" x2=\"14\" y1=\"2\" y2=\"2\"/><line x1=\"12\" x2=\"15\" y1=\"14\" y2=\"11\"/><circle cx=\"12\" cy=\"14\" r=\"8\"/>",
  "play": "<path d=\"M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z\"/>",
  "circle-play": "<path d=\"M9 9.003a1 1 0 0 1 1.517-.859l4.997 2.997a1 1 0 0 1 0 1.718l-4.997 2.997A1 1 0 0 1 9 14.996z\"/><circle cx=\"12\" cy=\"12\" r=\"10\"/>",
  "pause": "<rect x=\"14\" y=\"3\" width=\"5\" height=\"18\" rx=\"1\"/><rect x=\"5\" y=\"3\" width=\"5\" height=\"18\" rx=\"1\"/>",
  "circle-pause": "<circle cx=\"12\" cy=\"12\" r=\"10\"/><line x1=\"10\" x2=\"10\" y1=\"15\" y2=\"9\"/><line x1=\"14\" x2=\"14\" y1=\"15\" y2=\"9\"/>",
  "square": "<rect width=\"18\" height=\"18\" x=\"3\" y=\"3\" rx=\"2\"/>",
  "circle-stop": "<circle cx=\"12\" cy=\"12\" r=\"10\"/><rect x=\"9\" y=\"9\" width=\"6\" height=\"6\" rx=\"1\"/>",
  "bell": "<path d=\"M10.268 21a2 2 0 0 0 3.464 0\"/><path d=\"M3.262 15.326A1 1 0 0 0 4 17h16a1 1 0 0 0 .74-1.673C19.41 13.956 18 12.499 18 8A6 6 0 0 0 6 8c0 4.499-1.411 5.956-2.738 7.326\"/>",
  "bell-off": "<path d=\"M10.268 21a2 2 0 0 0 3.464 0\"/><path d=\"M17 17H4a1 1 0 0 1-.74-1.673C4.59 13.956 6 12.499 6 8a6 6 0 0 1 .258-1.742\"/><path d=\"m2 2 20 20\"/><path d=\"M8.668 3.01A6 6 0 0 1 18 8c0 2.687.77 4.653 1.707 6.05\"/>",
  "sparkles": "<path d=\"M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z\"/><path d=\"M20 2v4\"/><path d=\"M22 4h-4\"/><circle cx=\"4\" cy=\"20\" r=\"2\"/>",
  "wand-sparkles": "<path d=\"m21.64 3.64-1.28-1.28a1.21 1.21 0 0 0-1.72 0L2.36 18.64a1.21 1.21 0 0 0 0 1.72l1.28 1.28a1.2 1.2 0 0 0 1.72 0L21.64 5.36a1.2 1.2 0 0 0 0-1.72\"/><path d=\"m14 7 3 3\"/><path d=\"M5 6v4\"/><path d=\"M19 14v4\"/><path d=\"M10 2v2\"/><path d=\"M7 8H3\"/><path d=\"M21 16h-4\"/><path d=\"M11 3H9\"/>",
  "brain": "<path d=\"M12 18V5\"/><path d=\"M15 13a4.17 4.17 0 01-3-4 4.17 4.17 0 01-3 4\"/><path d=\"M17.707 6.294A3 3 0 1012 5a3 3 0 10-5.707 1.294\"/><path d=\"M17.996 5.125a4 4 0 012.526 5.77\"/><path d=\"M18 18a4 4 0 002-7.465\"/><path d=\"M19.967 17.482a4 4 0 11-7.967.517 4 4 0 11-7.967-.517\"/><path d=\"M6 18a4 4 0 01-2-7.465\"/><path d=\"M6.004 5.125a4 4 0 00-2.526 5.77\"/>",
  "hammer": "<path d=\"m15 12-9.373 9.373a1 1 0 0 1-3.001-3L12 9\"/><path d=\"m18 15 4-4\"/><path d=\"m21.5 11.5-1.914-1.914A2 2 0 0 1 19 8.172v-.344a2 2 0 0 0-.586-1.414l-1.657-1.657A6 6 0 0 0 12.516 3H9l1.243 1.243A6 6 0 0 1 12 8.485V10l2 2h1.172a2 2 0 0 1 1.414.586L18.5 14.5\"/>",
  "wrench": "<path d=\"M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.106-3.105c.32-.322.863-.22.983.218a6 6 0 0 1-8.259 7.057l-7.91 7.91a1 1 0 0 1-2.999-3l7.91-7.91a6 6 0 0 1 7.057-8.259c.438.12.54.662.219.984z\"/>",
  "settings": "<path d=\"M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915\"/><circle cx=\"12\" cy=\"12\" r=\"3\"/>",
  "terminal": "<path d=\"M12 19h8\"/><path d=\"m4 17 6-6-6-6\"/>",
  "folder": "<path d=\"M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z\"/>",
  "file-text": "<path d=\"M6 22a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h8a2.4 2.4 0 0 1 1.704.706l3.588 3.588A2.4 2.4 0 0 1 20 8v12a2 2 0 0 1-2 2z\"/><path d=\"M14 2v5a1 1 0 0 0 1 1h5\"/><path d=\"M10 9H8\"/><path d=\"M16 13H8\"/><path d=\"M16 17H8\"/>",
  "image": "<rect width=\"18\" height=\"18\" x=\"3\" y=\"3\" rx=\"2\" ry=\"2\"/><circle cx=\"9\" cy=\"9\" r=\"2\"/><path d=\"m21 15-3.086-3.086a2 2 0 0 0-2.828 0L6 21\"/>",
  "monitor": "<rect width=\"20\" height=\"14\" x=\"2\" y=\"3\" rx=\"2\"/><line x1=\"8\" x2=\"16\" y1=\"21\" y2=\"21\"/><line x1=\"12\" x2=\"12\" y1=\"17\" y2=\"21\"/>",
  "network": "<rect x=\"16\" y=\"16\" width=\"6\" height=\"6\" rx=\"1\"/><rect x=\"2\" y=\"16\" width=\"6\" height=\"6\" rx=\"1\"/><rect x=\"9\" y=\"2\" width=\"6\" height=\"6\" rx=\"1\"/><path d=\"M5 16v-3a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v3\"/><path d=\"M12 12V8\"/>",
  "cloud-upload": "<path d=\"M12 13v8\"/><path d=\"M4 14.899A7 7 0 1 1 15.71 8h1.79a4.5 4.5 0 0 1 2.5 8.242\"/><path d=\"m8 17 4-4 4 4\"/>",
  "cloud": "<path d=\"M17.5 19H9a7 7 0 1 1 6.71-9h1.79a4.5 4.5 0 1 1 0 9Z\"/>",
  "leaf": "<path d=\"M11 20a10 10 0 0010-10 25.9 25.9 0 00-1.04-7.281 1 1 0 00-1.755-.325C15.833 5.5 13 5.5 9.8 6.1A7 7 0 0011 20\"/><path d=\"M2 21a5 5 0 012.911-4.544C7.613 15.212 8.351 15.24 11 13\"/>",
  "bug": "<path d=\"M12 20v-9\"/><path d=\"M14 7a4 4 0 0 1 4 4v3a6 6 0 0 1-12 0v-3a4 4 0 0 1 4-4z\"/><path d=\"M14.12 3.88 16 2\"/><path d=\"M21 21a4 4 0 0 0-3.81-4\"/><path d=\"M21 5a4 4 0 0 1-3.55 3.97\"/><path d=\"M22 13h-4\"/><path d=\"M3 21a4 4 0 0 1 3.81-4\"/><path d=\"M3 5a4 4 0 0 0 3.55 3.97\"/><path d=\"M6 13H2\"/><path d=\"m8 2 1.88 1.88\"/><path d=\"M9 7.13V6a3 3 0 1 1 6 0v1.13\"/>",
  "star": "<path d=\"M11.525 2.295a.53.53 0 0 1 .95 0l2.31 4.679a2.123 2.123 0 0 0 1.595 1.16l5.166.756a.53.53 0 0 1 .294.904l-3.736 3.638a2.123 2.123 0 0 0-.611 1.878l.882 5.14a.53.53 0 0 1-.771.56l-4.618-2.428a2.122 2.122 0 0 0-1.973 0L6.396 21.01a.53.53 0 0 1-.77-.56l.881-5.139a2.122 2.122 0 0 0-.611-1.879L2.16 9.795a.53.53 0 0 1 .294-.906l5.165-.755a2.122 2.122 0 0 0 1.597-1.16z\"/>",
  "flag": "<path d=\"M4 22V4a1 1 0 0 1 .4-.8A6 6 0 0 1 8 2c3 0 5 2 7.333 2q2 0 3.067-.8A1 1 0 0 1 20 4v10a1 1 0 0 1-.4.8A6 6 0 0 1 16 16c-3 0-5-2-8-2a6 6 0 0 0-4 1.528\"/>",
  "heart": "<path d=\"M2 9.5a5.5 5.5 0 0 1 9.591-3.676.56.56 0 0 0 .818 0A5.49 5.49 0 0 1 22 9.5c0 2.29-1.5 4-3 5.5l-5.492 5.313a2 2 0 0 1-3 .019L5 15c-1.5-1.5-3-3.2-3-5.5\"/>",
  "user": "<path d=\"M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2\"/><circle cx=\"12\" cy=\"7\" r=\"4\"/>",
  "users": "<path d=\"M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2\"/><path d=\"M16 3.128a4 4 0 0 1 0 7.744\"/><path d=\"M22 21v-2a4 4 0 0 0-3-3.87\"/><circle cx=\"9\" cy=\"7\" r=\"4\"/>",
  "hand": "<path d=\"M18 11V6a2 2 0 0 0-2-2a2 2 0 0 0-2 2\"/><path d=\"M14 10V4a2 2 0 0 0-2-2a2 2 0 0 0-2 2v2\"/><path d=\"M10 10.5V6a2 2 0 0 0-2-2a2 2 0 0 0-2 2v8\"/><path d=\"M18 8a2 2 0 1 1 4 0v6a8 8 0 0 1-8 8h-2c-2.8 0-4.5-.86-5.99-2.34l-3.6-3.6a2 2 0 0 1 2.83-2.82L7 15\"/>",
  "eye": "<path d=\"M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0\"/><circle cx=\"12\" cy=\"12\" r=\"3\"/>",
  "lock": "<rect width=\"18\" height=\"11\" x=\"3\" y=\"11\" rx=\"2\" ry=\"2\"/><path d=\"M7 11V7a5 5 0 0 1 10 0v4\"/>",
  "key-round": "<path d=\"M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z\"/><circle cx=\"16.5\" cy=\"7.5\" r=\".5\" fill=\"currentColor\"/>",
  "globe": "<circle cx=\"12\" cy=\"12\" r=\"10\"/><path d=\"M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20\"/><path d=\"M2 12h20\"/>",
  "search": "<path d=\"m21 21-4.34-4.34\"/><circle cx=\"11\" cy=\"11\" r=\"8\"/>",
  "refresh-cw": "<path d=\"M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8\"/><path d=\"M21 3v5h-5\"/><path d=\"M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16\"/><path d=\"M8 16H3v5\"/>",
  "git-branch": "<path d=\"M15 6a9 9 0 0 0-9 9V3\"/><circle cx=\"18\" cy=\"6\" r=\"3\"/><circle cx=\"6\" cy=\"18\" r=\"3\"/>",
  "git-pull-request": "<circle cx=\"18\" cy=\"18\" r=\"3\"/><circle cx=\"6\" cy=\"6\" r=\"3\"/><path d=\"M13 6h3a2 2 0 0 1 2 2v7\"/><line x1=\"6\" x2=\"6\" y1=\"9\" y2=\"21\"/>",
  "arrow-up": "<path d=\"m5 12 7-7 7 7\"/><path d=\"M12 19V5\"/>",
  "arrow-down": "<path d=\"M12 5v14\"/><path d=\"m19 12-7 7-7-7\"/>",
  "send": "<path d=\"M14.536 21.686a.5.5 0 0 0 .937-.024l6.5-19a.496.496 0 0 0-.635-.635l-19 6.5a.5.5 0 0 0-.024.937l7.93 3.18a2 2 0 0 1 1.112 1.11z\"/><path d=\"m21.854 2.147-10.94 10.939\"/>",
  "inbox": "<polyline points=\"22 12 16 12 14 15 10 15 8 12 2 12\"/><path d=\"M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z\"/>",
  "trash-2": "<path d=\"M10 11v6\"/><path d=\"M14 11v6\"/><path d=\"M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6\"/><path d=\"M3 6h18\"/><path d=\"M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2\"/>",
  "pencil": "<path d=\"M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z\"/><path d=\"m15 5 4 4\"/>",
  "link": "<path d=\"M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71\"/><path d=\"M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71\"/>",
  "cpu": "<path d=\"M12 20v2\"/><path d=\"M12 2v2\"/><path d=\"M17 20v2\"/><path d=\"M17 2v2\"/><path d=\"M2 12h2\"/><path d=\"M2 17h2\"/><path d=\"M2 7h2\"/><path d=\"M20 12h2\"/><path d=\"M20 17h2\"/><path d=\"M20 7h2\"/><path d=\"M7 20v2\"/><path d=\"M7 2v2\"/><rect x=\"4\" y=\"4\" width=\"16\" height=\"16\" rx=\"2\"/><rect x=\"8\" y=\"8\" width=\"8\" height=\"8\" rx=\"1\"/>",
  "server": "<rect width=\"20\" height=\"8\" x=\"2\" y=\"2\" rx=\"2\" ry=\"2\"/><rect width=\"20\" height=\"8\" x=\"2\" y=\"14\" rx=\"2\" ry=\"2\"/><line x1=\"6\" x2=\"6.01\" y1=\"6\" y2=\"6\"/><line x1=\"6\" x2=\"6.01\" y1=\"18\" y2=\"18\"/>",
  "message-square": "<path d=\"M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z\"/>",
  "book-open": "<path d=\"M12 5v16\"/><path d=\"M20.001 19A2 2 0 0022 17V5a2 2 0 00-1.999-2L16 3.002A5 5 0 0012 5a5 5 0 00-4-2H4a2 2 0 00-2 2v12a2 2 0 001.999 2H8a5 5 0 014 2 5 5 0 014-2z\"/>",
  "list": "<path d=\"M3 5h.01\"/><path d=\"M3 12h.01\"/><path d=\"M3 19h.01\"/><path d=\"M8 5h13\"/><path d=\"M8 12h13\"/><path d=\"M8 19h13\"/>",
  "chart-column": "<path d=\"M3 3v16a2 2 0 0 0 2 2h16\"/><path d=\"M18 17V9\"/><path d=\"M13 17V5\"/><path d=\"M8 17v-3\"/>",
  "gauge": "<path d=\"m12 14 4-4\"/><path d=\"M3.34 19a10 10 0 1 1 17.32 0\"/>",
  "plus": "<path d=\"M5 12h14\"/><path d=\"M12 5v14\"/>",
  "minus": "<path d=\"M5 12h14\"/>",
  "circle": "<circle cx=\"12\" cy=\"12\" r=\"10\"/>",
  "waypoints": "<path d=\"m10.586 5.414-5.172 5.172\"/><path d=\"m18.586 13.414-5.172 5.172\"/><path d=\"M6 12h12\"/><circle cx=\"12\" cy=\"20\" r=\"2\"/><circle cx=\"12\" cy=\"4\" r=\"2\"/><circle cx=\"20\" cy=\"12\" r=\"2\"/><circle cx=\"4\" cy=\"12\" r=\"2\"/>",
  "copy": "<rect width=\"14\" height=\"14\" x=\"8\" y=\"8\" rx=\"2\" ry=\"2\"/><path d=\"M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2\"/>",
  "coffee": "<path d=\"M10 2v2\"/><path d=\"M14 2v2\"/><path d=\"M16 8a1 1 0 0 1 1 1v8a4 4 0 0 1-4 4H7a4 4 0 0 1-4-4V9a1 1 0 0 1 1-1h14a4 4 0 1 1 0 8h-1\"/><path d=\"M6 2v2\"/>",
  "moon": "<path d=\"M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401\"/>",
  "sun": "<circle cx=\"12\" cy=\"12\" r=\"4\"/><path d=\"M12 2v2\"/><path d=\"M12 20v2\"/><path d=\"m4.93 4.93 1.41 1.41\"/><path d=\"m17.66 17.66 1.41 1.41\"/><path d=\"M2 12h2\"/><path d=\"M20 12h2\"/><path d=\"m6.34 17.66-1.41 1.41\"/><path d=\"m19.07 4.93-1.41 1.41\"/>",
  "package": "<path d=\"M11 21.73a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73z\"/><path d=\"M12 22V12\"/><polyline points=\"3.29 7 12 12 20.71 7\"/><path d=\"m7.5 4.27 9 5.15\"/>",
  "tag": "<path d=\"M12.586 2.586A2 2 0 0 0 11.172 2H4a2 2 0 0 0-2 2v7.172a2 2 0 0 0 .586 1.414l8.704 8.704a2.426 2.426 0 0 0 3.42 0l6.58-6.58a2.426 2.426 0 0 0 0-3.42z\"/><circle cx=\"7.5\" cy=\"7.5\" r=\".5\" fill=\"currentColor\"/>",
  "calendar": "<path d=\"M8 2v3\"/><path d=\"M16 2v3\"/><rect x=\"3\" y=\"3\" width=\"18\" height=\"18\" rx=\"2\"/><path d=\"M3 9h18\"/>",
  "mail": "<path d=\"m22 7-8.991 5.727a2 2 0 0 1-2.009 0L2 7\"/><rect x=\"2\" y=\"4\" width=\"20\" height=\"16\" rx=\"2\"/>",
  "phone": "<path d=\"M13.832 16.568a1 1 0 0 0 1.213-.303l.355-.465A2 2 0 0 1 17 15h3a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2A18 18 0 0 1 2 4a2 2 0 0 1 2-2h3a2 2 0 0 1 2 2v3a2 2 0 0 1-.8 1.6l-.468.351a1 1 0 0 0-.292 1.233 14 14 0 0 0 6.392 6.384\"/>",
  "wifi": "<path d=\"M12 20h.01\"/><path d=\"M2 8.82a15 15 0 0 1 20 0\"/><path d=\"M5 12.859a10 10 0 0 1 14 0\"/><path d=\"M8.5 16.429a5 5 0 0 1 7 0\"/>",
};
const FILLABLE = new Set(["zap", "bell", "star", "flag", "heart", "play", "circle", "square", "leaf"]);
// A status icon as cmux's sidebar draws it: "emoji:…" and "text:…" as text,
// anything else an SF Symbol name ("sf:" optional). Names we have no drawing
// for get no icon, as the Mac shows none for a name it does not know.
function statusIcon(icon) {
  const raw = String(icon || "");
  if (raw.startsWith("emoji:")) return el("span", { className: "d-ic", textContent: raw.slice(6) });
  if (raw.startsWith("text:")) return el("span", { className: "d-ic d-ic-text", textContent: raw.slice(5) });
  const name = raw.replace(/^sf:/, "");
  const parts = name.split(".").filter((p) => p !== "fill");
  let lucide = "";
  while (parts.length && !(lucide = SF_TO_LUCIDE[parts.join(".")] || "")) parts.pop();
  if (!lucide) return null;
  const fill = /\.fill\b/.test(name) && FILLABLE.has(lucide) ? "currentColor" : "none";
  const span = el("span", { className: "d-ic" });
  span.innerHTML = `<svg viewBox="0 0 24 24" fill="${fill}" stroke="currentColor" stroke-width="2.25" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${LUCIDE[lucide]}</svg>`;
  return span;
}
function shortPath(path, lastOnly) {
  if (!path || !lastOnly) return path || "";
  const parts = path.split("/").filter(Boolean);
  return parts.length > 1 ? "…/" + parts[parts.length - 1] : path;
}
const LOG_COLORS = { error: "var(--err)", warning: "var(--warn)", success: "var(--ok)", progress: "var(--blue)" };
function wsDetails(ws, attn, opt) {
  const out = [];
  const sb = ws.sidebar || {};
  if (attn.needsInput) out.push(el("div", { className: "sub attn", textContent: "Needs your input" }));
  if (opt.hideAllDetails) return out;
  if (opt.showWorkspaceDescription && ws.description) {
    out.push(el("div", { className: "d-desc", innerHTML: mdInline(String(ws.description)) }));
  }
  if (opt.showNotificationMessage && ws.latest_notification_text) {
    const note = el("div", { className: "d-note", textContent: plainMarkdown(String(ws.latest_notification_text)).trim() });
    // The Mac's limit (12 lines by default) fits a tall sidebar; on a phone it
    // leaves room for two or three workspaces. Show at most 3 lines, as the
    // notification list does, and the Mac's limit once tapped.
    const limit = Math.max(1, opt.notificationMessageLineLimit || 12);
    const open = state.openNotes.has(ws.id);
    note.style.webkitLineClamp = note.style.lineClamp = String(open ? limit : Math.min(limit, 3));
    note.onclick = (e) => {
      if (!open && note.scrollHeight <= note.clientHeight + 1) return;  // nothing hidden: open the workspace
      e.stopPropagation();
      if (open) state.openNotes.delete(ws.id); else state.openNotes.add(ws.id);
      renderNav();
    };
    out.push(note);
  }
  if (opt.showSSH && ws.remote) {
    out.push(el("div", { className: "d-line mono" }, el("span", { className: "grow", textContent: ws.remote.target }),
      ws.remote.state ? el("span", { className: "d-dim", textContent: String(ws.remote.state) }) : null));
  }
  if (opt.showCustomMetadata) {
    const statuses = sb.statuses || [];
    const expanded = state.moreDetails.has(ws.id);
    for (const st of expanded ? statuses : statuses.slice(0, 3)) {
      const link = /^https?:\/\//i.test(st.url || "") ? st.url : "";
      const line = el(link ? "a" : "div", { className: "d-line d-status" },
        statusIcon(st.icon),
        st.format === "markdown"
          ? el("span", { className: "grow", innerHTML: mdInline(String(st.value || "")) })
          : el("span", { className: "grow", textContent: st.value }));
      if (st.color) line.style.color = st.color;
      if (link) { line.href = link; line.target = "_blank"; line.rel = "noopener"; line.onclick = (e) => e.stopPropagation(); }
      out.push(line);
    }
    if (statuses.length > 3) out.push(moreToggle(ws, expanded ? "Show less" : "Show more"));
    const blocks = sb.blocks || [];
    for (const b of expanded ? blocks : blocks.slice(0, 1)) out.push(el("div", { className: "d-block msg", innerHTML: md(b.markdown || "") }));
    if (blocks.length > 1 && statuses.length <= 3) out.push(moreToggle(ws, expanded ? "Show less details" : "Show more details"));
  }
  if (opt.showLog && sb.log && sb.log.message) {
    const line = el("div", { className: "d-line d-log" }, el("span", { className: "grow", textContent: oneLine(sb.log.message) }));
    if (LOG_COLORS[sb.log.level]) line.style.color = LOG_COLORS[sb.log.level];
    out.push(line);
  }
  if (opt.showProgress && sb.progress && typeof sb.progress.value === "number") {
    const pct = Math.max(0, Math.min(1, sb.progress.value)) * 100;
    out.push(el("div", { className: "d-progress" },
      el("div", { className: "bar" }, el("div", { className: "fill", style: `width:${pct}%` })),
      sb.progress.label ? el("div", { className: "d-dim", textContent: sb.progress.label }) : null));
  }
  if (opt.showBranchDirectory && (ws.branch || ws.directory)) {
    const branch = ws.branch ? "⎇ " + ws.branch + (ws.git_dirty ? "*" : "") : "";
    const dir = shortPath(ws.directory, opt.pathLastSegmentOnly);
    if (opt.stackBranchDirectory) {
      if (branch) out.push(el("div", { className: "d-line mono", textContent: branch }));
      if (dir) out.push(el("div", { className: "d-line mono", textContent: dir }));
    } else {
      out.push(el("div", { className: "d-line mono", textContent: [branch, dir].filter(Boolean).join(" · ") }));
    }
  }
  if (opt.showPullRequests) {
    const pr = sb.pr;
    const urls = [...new Set([pr && pr.url, ...(ws.pull_requests || [])].filter((u) => /^https?:\/\//i.test(u || "")))];
    for (const url of urls) {
      const num = (url.match(/\/pull\/(\d+)/) || [])[1] || (pr && pr.url === url ? pr.number : "");
      const text = [(pr && pr.url === url && pr.label) || "PR", num ? "#" + num : "", pr && pr.url === url ? pr.state : ""].filter(Boolean).join(" ");
      out.push(el("a", { className: "d-line mono d-pr", href: url, target: "_blank", rel: "noopener", textContent: text, onclick: (e) => e.stopPropagation() }));
    }
  }
  if (opt.showPorts && (ws.ports || []).length) {
    out.push(el("div", { className: "d-line mono d-dim", textContent: ws.ports.map((p) => ":" + p).join("  ") }));
  }
  return out;
}
function moreToggle(ws, label) {
  return el("button", { className: "d-more", textContent: label, onclick: (e) => {
    e.stopPropagation();
    if (state.moreDetails.has(ws.id)) state.moreDetails.delete(ws.id); else state.moreDetails.add(ws.id);
    renderNav();
  } });
}

// ---- main column ------------------------------------------------------------------
function renderMain() {
  const sel = state.sel || {};
  const { ws, surface } = selected();
  const view = sel.view === "new" ? "new" : state.pendingNew && sel.wsId === state.pendingNew.wsId ? "starting" : viewFor(surface);
  const key = view + ":" + (surface ? surface.id : sel.wsId || "");
  renderHeader(ws, surface, view);
  if (key === state.mounted) { updateComposer(view, surface); return; }
  unmountView();
  state.mounted = key;
  const root = $("view");
  root.replaceChildren();
  $("urlbar").hidden = view !== "browser";
  const reading = view === "browser" && readerOn.has(surface.id);
  $("readerBtn").classList.toggle("on", reading);
  $("readerBtn").title = reading ? "Show the screenshot" : "Reader view";
  if (view === "chat") mountChat(surface, root);
  else if (view === "terminal") mountTerminal(surface, root);
  else if (view === "browser") (readerOn.has(surface.id) ? mountReader : mountBrowser)(surface, root);
  else if (view === "file") mountFile(surface, root);
  else if (view === "new") mountNew(root);
  else if (view === "starting") root.append(el("div", { className: "empty" }, el("div", { className: "spin", style: "margin:0 auto 10px" }), "Starting Claude…"));
  else if (view === "unsupported") root.append(el("div", { className: "empty", textContent: `${surface.type} panels are not shown here.` }));
  else root.append(el("div", { className: "empty", textContent: ws ? "This workspace is empty." : "Pick a workspace." }));
  updateComposer(view, surface);
  restartPolling();
  restartChatPolls();
}

function unmountView() {
  if (state.browser) {
    state.browser.abort.abort();
    api("/api/browser/release", { surface_id: state.browser.sid }).catch(() => {});
    cancelAnimationFrame(state.browser.glide);
    if (state.browser.objectUrl) URL.revokeObjectURL(state.browser.objectUrl);
    state.browser = null;
  }
  if (state.file) { state.file.abort.abort(); state.file = null; }
}

function renderHeader(ws, surface, view) {
  // The workspace is what you look for in the list; the panel goes underneath.
  const wsName = ws ? ws.title || ws.ref : "";
  const panel = surface ? itemLabel(surface) : "";
  $("title").textContent = view === "new" ? "New Claude session" : wsName || panel || "psycommux";
  $("subtitle").textContent = view === "new" ? "" : ws ? [panel && panel !== wsName && panel !== ws.directory ? panel : null, ws.directory, ws.branch && ws.branch + (ws.git_dirty ? "*" : "")].filter(Boolean).join(" · ") : "";
  const actions = $("actions");
  actions.replaceChildren();
  if (surface && surface.agent && surface.agent.chat) {
    const toChat = view !== "chat";
    const b = el("button", { textContent: toChat ? "Chat" : "Terminal", title: toChat ? "Show the conversation" : "Show the terminal screen" });
    b.onclick = () => { state.viewBySurface[surface.id] = toChat ? "chat" : "terminal"; save("cmuxViews", state.viewBySurface); renderMain(); };
    actions.append(b);
  }
  if (view === "terminal" && PHONE.matches) {
    const b = el("button", { textContent: state.fontMode === "fit" ? "Aa+" : "Aa−", title: "Text size" });
    b.onclick = () => { state.fontMode = state.fontMode === "fit" ? "read" : "fit"; save("cmuxFontMode", state.fontMode); renderHeader(ws, surface, view); document.querySelectorAll("#view pre.term").forEach(restoreScroll); };
    actions.append(b);
  }
  if (ws && view !== "new") {
    const more = el("button", { className: "icon", textContent: "⋯", title: "Panel and workspace actions" });
    more.onclick = (e) => { e.stopPropagation(); showMenu(more, surface ? panelMenu(ws, surface) : workspaceMenu(ws)); };
    actions.append(more);
  }
}

// Menu entries: [label, fn, "danger"?], or null for a separator.
function showMenu(anchor, entries) {
  const pop = $("menuPop");
  pop.replaceChildren(...entries.map((entry) => {
    if (!entry) return el("div", { className: "sep" });
    const [label, fn, kind] = entry;
    const b = el("button", { textContent: label, className: kind || "" });
    b.onclick = (e) => { e.stopPropagation(); pop.hidden = true; fn(); };
    return b;
  }));
  pop.style.visibility = "hidden";
  pop.hidden = false;
  const r = anchor.getBoundingClientRect();
  const h = pop.offsetHeight;
  const below = r.bottom + 4 + h <= innerHeight - 8;
  pop.style.top = (below ? r.bottom + 4 : Math.max(8, r.top - 4 - h)) + "px";
  pop.style.right = Math.max(8, innerWidth - r.right) + "px";
  pop.style.left = "";
  pop.style.visibility = "";
}

// ---- dialogs: a sheet in the page, not window.confirm/prompt ----------------------
// ask({title, message, input, confirm, danger}) resolves to the typed text (or true), or null.
function ask({ title, message, input, confirm = "OK", danger = false, info = false }) {
  return new Promise((done) => {
    const scrim = el("div", { className: "ask-scrim" });
    const field = input !== undefined ? el("input", { className: "ask-input", value: input, autocapitalize: "off", autocorrect: "off", spellcheck: false, enterKeyHint: "done" }) : null;
    const finish = (value) => { scrim.remove(); done(value); };
    const ok = el("button", { className: "ask-ok" + (danger ? " danger" : ""), textContent: confirm });
    const cancel = el("button", { className: "ask-cancel", textContent: "Cancel" });
    ok.onclick = () => finish(field ? field.value.trim() || null : true);
    cancel.onclick = () => finish(null);
    scrim.onclick = (e) => { if (e.target === scrim) finish(null); };
    if (field) field.onkeydown = (e) => { if (e.key === "Enter" && !e.isComposing) { e.preventDefault(); ok.click(); } if (e.key === "Escape") cancel.click(); };
    scrim.append(el("div", { className: "ask" },
      el("div", { className: "ask-title", textContent: title }),
      message ? el("div", { className: "ask-msg", textContent: message }) : null,
      field,
      el("div", { className: "ask-buttons" }, info ? null : cancel, ok)));
    document.body.append(scrim);
    // Never leave a destructive button focused, where Return would press it.
    if (field) { field.focus(); field.select(); } else (danger ? cancel : ok).focus();
  });
}

// ---- workspace and panel actions ------------------------------------------------------
function failed(e) { ask({ title: "That did not work", message: e.message, info: true }); }
async function wsAction(ws, action, extra = {}) {
  try { await api("/api/workspace/action", { workspace_id: ws.id, action, ...extra }); }
  catch (e) { failed(e); return false; }
  scheduleTree(150);
  return true;
}
async function panelAction(surface, action, extra = {}) {
  try { return await api("/api/surface/action", { surface_id: surface.id, action, ...extra }); }
  catch (e) { failed(e); return null; }
  finally { scheduleTree(150); }
}
// Close; when cmux says something is still running there, ask once more and force it.
async function closeThing(kind, name, call, note = "") {
  if (!(await ask({ title: `Close ${kind}?`, message: `“${name}” will be closed on the Mac.${note}`, confirm: "Close", danger: true }))) return false;
  try { await call(false); return true; }
  catch (e) {
    if (e.code !== "confirmation_required") { failed(e); return false; }
  }
  const sure = await ask({ title: "Something is still running", message: `A program is running in “${name}” (Claude, a server, a build…). Closing ends it.`, confirm: "End it and close", danger: true });
  if (!sure) return false;
  try { await call(true); return true; } catch (e) { failed(e); return false; }
}
// The tree refresh notices the panel or workspace is gone and opens another.
function afterClose() { scheduleTree(100); }
function workspaceMenu(ws) {
  const name = ws.title || ws.ref;
  const items = wsItems(ws);
  const last = items[items.length - 1];
  const panes = (ws.panes || []).length;
  const entries = [
    ["Rename…", async () => {
      const title = await ask({ title: "Rename workspace", input: name, confirm: "Rename" });
      if (title && title !== name) wsAction(ws, "rename", { title });
    }],
    [ws.pinned ? "Unpin" : "Pin", () => wsAction(ws, ws.pinned ? "unpin" : "pin")],
    [unreadFor(ws) ? "Mark as read" : "Mark as unread", () => wsAction(ws, unreadFor(ws) ? "mark-read" : "mark-unread")],
    null,
  ];
  if (last) entries.push(["New terminal", () => panelAction(last, "new-terminal-right")]);
  if (ws.directory) entries.push(["New Claude session in this folder", () => { save("cmuxLastDir", ws.directory); select(ws.id, null, "new"); }]);
  entries.push(["Show on Mac", async () => { await api("/api/workspace/select", { workspace_id: ws.id }).catch(failed); scheduleTree(100); }]);
  if (panes > 1) entries.push(["Even out splits on Mac", () => wsAction(ws, "equalize")]);
  entries.push(null, ["Close workspace…", async () => {
    // cmux keeps pinned workspaces open; unpin first, as you would on the Mac.
    const close = async (force) => {
      if (ws.pinned) { await api("/api/workspace/action", { workspace_id: ws.id, action: "unpin" }); ws.pinned = false; }
      return api("/api/workspace/action", { workspace_id: ws.id, action: "close", force });
    };
    if (await closeThing("workspace", name, close, ws.pinned ? " It is pinned; it will be unpinned first." : "")) afterClose();
  }, "danger"]);
  return entries;
}
function panelMenu(ws, surface) {
  const name = itemLabel(surface);
  const entries = [
    ["Rename panel…", async () => {
      const title = await ask({ title: "Rename panel", input: name, confirm: "Rename" });
      if (title && title !== name) panelAction(surface, "rename", { title });
    }],
    ["New terminal tab", () => panelAction(surface, "new-terminal-right")],
    ["New browser tab…", async () => {
      const url = await ask({ title: "Open a web page", input: "https://", confirm: "Open" });
      if (url && url !== "https://") panelAction(surface, "new-browser-right", { url });
    }],
    ["Split right on Mac", () => panelAction(surface, "split-right")],
    ["Split down on Mac", () => panelAction(surface, "split-down")],
    ["Move to a new workspace", () => panelAction(surface, "move-to-new-workspace")],
  ];
  if (surface.type === "browser" && pageUrl(surface)) entries.push(["Open on this device", () => openHere(surface)]);
  entries.push(null, ["Workspace…", () => showMenu($("actions").lastElementChild || $("menu"), workspaceMenu(ws))]);
  entries.push(null, ["Close panel…", async () => {
    if (await closeThing("panel", name, (force) => api("/api/surface/action", { surface_id: surface.id, action: "close", force }))) afterClose();
  }, "danger"]);
  return entries;
}
document.addEventListener("click", () => { $("menuPop").hidden = true; document.body.classList.remove("show-notifs"); });

// Opening an item marks its notifications read, as focusing it in cmux does.
function markRead() {
  const { ws, surface } = selected();
  if (!ws) return;
  const unread = state.notifications.filter((n) => !n.is_read && n.workspace_id === ws.id);
  const forItem = surface && unread.some((n) => n.surface_id === surface.id);
  const forWs = unread.filter((n) => !n.surface_id);
  if (!forItem && !forWs.length) return;
  for (const n of unread) if (!n.surface_id || (surface && n.surface_id === surface.id)) n.is_read = true;
  renderNotifications();
  renderNav();
  const calls = [];
  if (forItem) calls.push(api("/api/notifications/read", { workspace_id: ws.id, surface_id: surface.id }));
  for (const n of forWs) calls.push(api("/api/notifications/read", { id: n.id }));
  Promise.allSettled(calls).then(() => scheduleTree());
}

// ---- views: chat and terminal ------------------------------------------------------
function mountChat(surface, root) {
  const chat = chatState(surface.agent.session_id);
  chat.surfaceId = surface.id;
  chat.root.dataset.session = surface.agent.session_id;
  chat.scroll.dataset.surface = surface.id + ":chat";
  if (!chat.loaded) chat.status.textContent = "Loading conversation…";
  root.append(chat.root);
  restoreScroll(chat.scroll);
}

function mountTerminal(surface, root) {
  const pre = el("pre", { className: "term" });
  pre.dataset.surface = surface.id;
  pre.addEventListener("scroll", () => rememberScroll(pre), { passive: true });
  root.append(pre);
  const cached = state.screens[surface.id];
  if (cached) paintScreen(pre, cached); else pre.textContent = "Loading…";
  restoreScroll(pre);
}

// ---- view: browser ------------------------------------------------------------------
// A live screenshot of the Mac's browser panel. Taps click there; drags scroll there, while the
// screenshot moves with the finger at once and settles as screenshots of the scrolled page arrive.
function mountBrowser(surface, root) {
  const img = el("img", { className: "shot", alt: "", draggable: false });
  const note = el("div", { className: "shot-note", textContent: "Loading page…" });
  const wrap = el("div", { className: "shot-wrap" }, img, note);
  root.append(wrap);
  $("url").value = surface.url || "";
  // offset: how far the screenshot is shifted (screen px): finger movement the shown screenshot does
  // not include yet. pending: movement not sent yet. acked: scrolls the page made, each with when it
  // made it, until a screenshot asked for after that shows them.
  const b = state.browser = {
    sid: surface.id, img, wrap, note, hash: "", fast: 0, abort: new AbortController(), objectUrl: "",
    offset: { x: 0, y: 0 }, pending: { x: 0, y: 0 }, origin: null, sending: false, acked: [], glide: 0,
  };
  attachBrowserGestures(b);
  browserLoop(b);
}

async function fetchShot(b) {
  const box = b.wrap.getBoundingClientRect();
  const res = await fetch("/api/browser/shot", {
    method: "POST", headers: { "Content-Type": "application/json" }, signal: b.abort.signal,
    body: JSON.stringify({ surface_id: b.sid, hash: b.hash, width: Math.round(box.width), height: Math.round(box.height) }),
  });
  if (res.status === 401) { location.href = "/"; throw new Error("unauthorized"); }
  if (!res.ok) {
    const data = await res.json().catch(() => ({}));
    throw new Error(data.message || data.error || res.statusText);
  }
  let url = res.headers.get("x-page-url") || "";
  try { url = decodeURI(url); } catch (e) { /* keep it encoded */ }
  return { hash: res.headers.get("x-shot-hash") || "", url, blob: res.status === 204 ? null : await res.blob() };
}

async function browserLoop(b) {
  while (state.browser === b) {
    await untilVisible(b);
    if (state.browser !== b) return;
    const asked = performance.now();
    try {
      const r = await fetchShot(b);
      if (state.browser !== b) return;
      if (r.blob) await showShot(b, r.blob);
      b.hash = r.hash;
      settleScroll(b, asked);
      if (r.url) b.url = r.url;
      if (r.url && document.activeElement !== $("url")) $("url").value = r.url;
    } catch (e) {
      if (state.browser !== b || e.name === "AbortError") return;
      b.note.hidden = false;
      b.note.textContent = "Browser unavailable: " + e.message;
      await sleep(3000);
    }
    await sleep(Date.now() < b.fast ? 150 : 1500);
  }
}

function showShot(b, blob) {
  const url = URL.createObjectURL(blob);
  return new Promise((done) => {
    b.img.onload = b.img.onerror = (e) => {
      if (b.objectUrl) URL.revokeObjectURL(b.objectUrl);
      b.objectUrl = url;
      b.note.hidden = e.type === "load";
      if (e.type === "error") b.note.textContent = "The screenshot did not load.";
      done();
    };
    b.img.src = url;
  });
}

// A screenshot asked for at `asked` shows every scroll the page made before then.
function settleScroll(b, asked) {
  const shown = b.acked.filter((a) => a.at < asked);
  if (!shown.length) return;
  b.acked = b.acked.filter((a) => a.at >= asked);
  for (const a of shown) { b.offset.x -= a.x; b.offset.y -= a.y; }
  placeShot(b);
}

function placeShot(b) {
  b.img.style.transform = b.offset.x || b.offset.y ? `translate(${b.offset.x}px, ${b.offset.y}px)` : "";
}

// Where the screenshot sits without its shift: the page's viewport as it is now.
function shotRect(b) {
  const r = b.img.getBoundingClientRect();
  return { left: r.left - b.offset.x, top: r.top - b.offset.y, width: r.width, height: r.height };
}

// The finger moved the content by (dx, dy) screen px.
function dragShot(b, dx, dy) {
  b.offset.x += dx; b.offset.y += dy;
  b.pending.x += dx; b.pending.y += dy;
  placeShot(b);
  sendScroll(b);
}

// One scroll request at a time; movement made meanwhile goes in the next, so scrolls keep pace with
// the round trip. The page answers how far it moved; what it could not (an edge) springs back.
async function sendScroll(b) {
  if (b.sending || !b.origin || (!b.pending.x && !b.pending.y)) return;
  const r = shotRect(b);
  if (!r.width || !r.height) return;
  const sent = b.pending;
  b.pending = { x: 0, y: 0 };
  b.sending = true;
  b.fast = Date.now() + 4000;
  try {
    const res = await api("/api/browser/act", { surface_id: b.sid, action: "scroll", fx: b.origin.fx, fy: b.origin.fy, dx: -sent.x / r.width, dy: -sent.y / r.height });
    const moved = res && res.value && typeof res.value === "object" ? res.value : { dx: -sent.x / r.width, dy: -sent.y / r.height };
    const did = { x: -moved.dx * r.width, y: -moved.dy * r.height };
    b.offset.x -= sent.x - did.x; b.offset.y -= sent.y - did.y;
    b.acked.push({ at: performance.now(), x: did.x, y: did.y });
  } catch (e) {
    b.offset.x -= sent.x; b.offset.y -= sent.y;
    b.note.hidden = false; b.note.textContent = e.message;
  }
  b.sending = false;
  if (state.browser !== b) return;
  placeShot(b);
  sendScroll(b);
}

function browserAct(body) {
  const b = state.browser;
  if (!b) return Promise.resolve();
  b.fast = Date.now() + 4000;
  return api("/api/browser/act", Object.assign({ surface_id: b.sid }, body)).catch((e) => { b.note.hidden = false; b.note.textContent = e.message; });
}

// Tap = click at that spot; drag = scroll the element under where it began, with a glide after a
// flick; wheel = scroll too.
function attachBrowserGestures(b) {
  let start = null;
  const frac = (x, y) => {
    const r = shotRect(b);
    return { fx: (x - r.left) / r.width, fy: (y - r.top) / r.height, r };
  };
  const stopGlide = () => { cancelAnimationFrame(b.glide); b.glide = 0; };
  b.wrap.addEventListener("pointerdown", (e) => {
    stopGlide();
    start = { x: e.clientX, y: e.clientY, lastX: e.clientX, lastY: e.clientY, axis: "", samples: [] };
    try { b.wrap.setPointerCapture(e.pointerId); } catch (err) { /* not a live pointer */ }
  });
  b.wrap.addEventListener("pointermove", (e) => {
    if (!start) return;
    if (!start.axis) {
      const ax = Math.abs(e.clientX - start.x), ay = Math.abs(e.clientY - start.y);
      if (Math.max(ax, ay) <= 8) return;
      // Most drags mean one direction; locking it keeps a vertical scroll from wobbling sideways.
      start.axis = ax > ay * 1.5 ? "x" : ay > ax * 1.5 ? "y" : "xy";
      const o = frac(start.x, start.y);
      b.origin = { fx: o.fx, fy: o.fy };
    }
    const dx = start.axis === "y" ? 0 : e.clientX - start.lastX;
    const dy = start.axis === "x" ? 0 : e.clientY - start.lastY;
    start.lastX = e.clientX; start.lastY = e.clientY;
    start.samples.push({ t: performance.now(), dx, dy });
    dragShot(b, dx, dy);
  });
  const end = (e) => {
    if (!start) return;
    const s = start; start = null;
    if (!s.axis) {
      if (e.type === "pointercancel") return;
      const { fx, fy, r } = frac(e.clientX, e.clientY);
      if (!r.width || !r.height || fx < 0 || fx > 1 || fy < 0 || fy > 1) return;
      const ring = el("div", { className: "tap-ring" });
      ring.style.left = e.clientX - b.wrap.getBoundingClientRect().left + "px";
      ring.style.top = e.clientY - b.wrap.getBoundingClientRect().top + "px";
      b.wrap.append(ring);
      setTimeout(() => ring.remove(), 600);
      browserAct({ action: "tap", fx, fy });
      return;
    }
    // Glide on at the finger's speed over its last 80 ms, slowing down.
    const now = performance.now();
    const recent = s.samples.filter((p) => now - p.t < 80);
    if (!recent.length) return;
    const span = Math.max(16, now - recent[0].t);
    let vx = recent.reduce((a, p) => a + p.dx, 0) / span, vy = recent.reduce((a, p) => a + p.dy, 0) / span;
    if (Math.hypot(vx, vy) < 0.3) return;
    let last = now;
    const step = (t) => {
      const dt = Math.min(32, t - last); last = t;
      const decay = Math.pow(0.996, dt);
      vx *= decay; vy *= decay;
      if (Math.hypot(vx, vy) < 0.05 || state.browser !== b) { b.glide = 0; return; }
      dragShot(b, vx * dt, vy * dt);
      b.glide = requestAnimationFrame(step);
    };
    b.glide = requestAnimationFrame(step);
  };
  b.wrap.addEventListener("pointerup", end);
  b.wrap.addEventListener("pointercancel", end);
  b.wrap.addEventListener("wheel", (e) => {
    e.preventDefault();
    stopGlide();
    const o = frac(e.clientX, e.clientY);
    if (!o.r.height) return;
    b.origin = { fx: o.fx, fy: o.fy };
    dragShot(b, -e.deltaX, -e.deltaY);
  }, { passive: false });
}

// ---- view: browser, reader mode ------------------------------------------------------------
// The page's main content laid out as the phone's own text: sharp at any pixel density and sized
// for the phone. The server sends a tree of plain tags (see READ_SCRIPT); elements are built from
// it one by one, never from the page's HTML. Links navigate the Mac's panel, and the new page is
// read in turn. It is how browser panels open; Aa switches to the screenshot, which is still the
// way to use forms, and ↗ opens the page in the phone's own browser.
const READER_TAGS = new Set(["h1", "h2", "h3", "h4", "h5", "h6", "p", "ul", "ol", "li", "blockquote", "a", "strong", "em", "b", "i", "u", "br", "hr", "table", "thead", "tbody", "tfoot", "tr", "th", "td", "figure", "figcaption", "del", "s", "sup", "sub", "dl", "dt", "dd", "kbd", "mark", "code", "pre", "caption", "small", "span", "div"]);

$("readerBtn").onclick = () => {
  const { surface } = selected();
  if (!surface || surface.type !== "browser") return;
  setReader(surface, !readerOn.has(surface.id));
};
function setReader(surface, on) {
  if (on) shotView.delete(surface.id); else shotView.add(surface.id);
  save("browserShot", [...shotView].slice(-50));
  state.mounted = "";
  render();
};

function mountReader(surface, root) {
  const note = el("div", { className: "reader-note", textContent: "Reading page…" });
  const doc = el("div", { className: "doc reader" });
  root.append(el("div", { className: "reader-wrap" }, note, doc));
  $("url").value = surface.url || "";
  const b = state.browser = { sid: surface.id, reader: true, doc, note, hash: "", url: "", fast: 0, abort: new AbortController(), glide: 0, objectUrl: "" };
  doc.addEventListener("click", (e) => {
    const a = e.target.closest("a[href]");
    if (!a) return;
    e.preventDefault();
    b.note.hidden = false;
    b.note.textContent = "Opening…";
    browserAct({ action: "navigate", url: a.href });
  });
  readerLoop(b);
}

async function readerLoop(b) {
  while (state.browser === b) {
    await untilVisible(b);
    if (state.browser !== b) return;
    try {
      const box = b.doc.getBoundingClientRect();
      const r = await api("/api/browser/act", { surface_id: b.sid, action: "read", hash: b.hash, width: Math.round(box.width), height: Math.round(box.height) }, b.abort.signal);
      if (state.browser !== b) return;
      if (!r.same) { b.hash = r.hash; showReader(b, r.page || {}); }
      if (b.note.textContent !== "Opening…" || !r.same) b.note.hidden = true;
    } catch (e) {
      if (state.browser !== b || e.name === "AbortError") return;
      b.note.hidden = false;
      b.note.textContent = "Browser unavailable: " + e.message;
      await sleep(3000);
    }
    await sleep(Date.now() < b.fast ? 500 : 3000);
  }
}

function showReader(b, page) {
  const samePage = page.url === b.url;
  const top = b.doc.scrollTop;
  if (page.url) b.url = page.url;
  if (page.url && document.activeElement !== $("url")) $("url").value = page.url;
  const article = el("article", { className: "msg bot reader-body" });
  const body = readerNode(page.body);
  if (body) article.append(body);
  // The tab title as a heading, unless the page already opens with it.
  const squash = (t) => (t || "").replace(/\s+/g, "");
  const title = squash(page.title).slice(0, 16);
  if (page.title && !article.querySelector("h1") && !squash(article.textContent.slice(0, 600)).includes(title)) {
    article.prepend(el("h1", { textContent: page.title }));
  }
  if (!body) {
    const surface = { id: b.sid, url: b.url };
    article.append(el("p", { className: "reader-empty", textContent: "Nothing to read on this page yet." }),
      el("div", { className: "reader-ways" },
        el("button", { textContent: "Show the screenshot", onclick: () => setReader(surface, false) }),
        /^https?:\/\//i.test(b.url) ? el("button", { textContent: "Open on this device ↗", onclick: () => openHere(surface) }) : null));
  }
  b.doc.replaceChildren(article);
  b.doc.scrollTop = samePage ? top : 0;
}

function readerNode(n) {
  if (typeof n === "string") return document.createTextNode(n);
  if (!Array.isArray(n)) return null;
  const [tag, attrs, ...kids] = n;
  if (tag === "img") {
    const src = attrs && typeof attrs.src === "string" ? attrs.src : "";
    const alt = attrs && typeof attrs.alt === "string" ? attrs.alt : "";
    // Only https: the page CSP allows no other remote images, and http would be mixed content.
    if (!/^https:\/\//i.test(src)) return alt ? document.createTextNode(alt) : null;
    const img = el("img", { alt, loading: "lazy", decoding: "async", referrerPolicy: "no-referrer" });
    const w = attrs && Number(attrs.w);
    if (w > 0) img.style.width = w + "px";
    if (w > 0 && w < 64) img.className = "inline";
    img.onerror = () => img.replaceWith(alt ? document.createTextNode(alt) : "");
    img.src = src;
    return img;
  }
  const name = READER_TAGS.has(tag) ? tag : "div";
  const node = document.createElement(name);
  if (name === "a" && attrs && /^https?:\/\//i.test(attrs.href || "")) node.href = attrs.href;
  if (name === "pre") node.className = "code whole";
  for (const k of kids) { const c = readerNode(k); if (c) node.append(c); }
  return name === "table" ? el("div", { className: "tablewrap" }, node) : node;
}

// The page's URL as last seen in a screenshot, which follows the page; the tree's lags behind.
function pageUrl(surface) {
  const live = state.browser && state.browser.sid === surface.id && state.browser.url;
  return live || surface.url || "";
}
// Only http(s): the URL comes from the page on the Mac.
function openHere(surface) {
  const url = pageUrl(surface);
  if (/^https?:\/\//i.test(url)) window.open(url, "_blank", "noopener");
}
$("openHere").onclick = () => { const { surface } = selected(); if (surface) openHere(surface); };

for (const btn of document.querySelectorAll("#urlbar [data-nav]")) btn.onclick = () => browserAct({ action: btn.dataset.nav });
$("url").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); browserAct({ action: "navigate", url: $("url").value }); $("url").blur(); }
});

// ---- view: Markdown and file previews ---------------------------------------------------
function mountFile(surface, root) {
  const doc = el("div", { className: "doc" }, el("div", { className: "empty", textContent: "Loading…" }));
  root.append(doc);
  const f = state.file = { sid: surface.id, doc, stamp: "", abort: new AbortController(), markdown: surface.type === "markdown" };
  fileLoop(f);
}

async function fileLoop(f) {
  while (state.file === f) {
    await untilVisible(f);
    try {
      const r = await api("/api/file", { surface_id: f.sid, stamp: f.stamp }, f.abort.signal);
      if (state.file !== f) return;
      if (!r.unchanged) {
        f.stamp = r.stamp;
        const markdown = f.markdown || /\.(md|markdown|mdx)$/i.test(r.name || "");
        const top = f.doc.scrollTop;
        f.doc.replaceChildren(markdown
          ? el("div", { className: "msg bot", innerHTML: md(r.text) })
          : el("pre", { className: "code whole", textContent: r.text }));
        if (r.truncated) f.doc.append(el("div", { className: "empty", textContent: "(file truncated)" }));
        f.doc.scrollTop = top;
        $("subtitle").textContent = r.path || $("subtitle").textContent;
      }
    } catch (e) {
      if (state.file !== f || e.name === "AbortError") return;
      f.doc.replaceChildren(el("div", { className: "empty", textContent: e.message }));
    }
    await sleep(2500);
  }
}

// ---- view: new Claude session -------------------------------------------------------------
function mountNew(root) {
  const dirs = [...new Set(allWorkspaces().map((w) => w.directory).filter(Boolean))].slice(0, 12);
  const cwd = el("input", { placeholder: "~/path/to/project", value: saved("cmuxLastDir", dirs[0] || "~"), autocapitalize: "off", spellcheck: false });
  const chips = el("div", { className: "dirs" });
  const mark = () => chips.querySelectorAll("button").forEach((b) => b.classList.toggle("on", b.textContent === cwd.value));
  for (const d of dirs) {
    const b = el("button", { textContent: d });
    b.onclick = () => { cwd.value = d; mark(); };
    chips.append(b);
  }
  cwd.addEventListener("input", mark);
  mark();
  root.append(el("div", { className: "newchat" },
    el("h1", { textContent: "What should Claude work on?" }),
    el("label", { textContent: "Folder" }), cwd, chips));
  state.newCwd = cwd;
  $("input").focus();
}

async function startSession(prompt) {
  const cwd = state.newCwd && state.newCwd.value.trim();
  if (!cwd) return;
  save("cmuxLastDir", cwd);
  const r = await api("/api/session/new", { cwd, prompt });
  const wsId = r.workspace_id;
  state.pendingNew = { wsId, until: Date.now() + 45000 };
  select(wsId, null);
  for (let i = 0; i < 30 && state.pendingNew; i++) { await sleep(1500); await loadTree(); }
}
$("newChat").onclick = () => select(state.sel && state.sel.wsId, null, "new");

// ---- dictation ----------------------------------------------------------------------------------
// The browser's speech recognition (Chrome sends audio to Google, Safari to Apple) types into the
// composer at the caret; the user reviews the text before sending. While it listens, a bar above the
// box shows a level animation, the elapsed time, the language (tap to switch) and a cancel button.
const Speech = window.SpeechRecognition || window.webkitSpeechRecognition;
// navigator.language is only the first system language, so a phone set to English with Chinese as a
// second language would get English recognition: offer Chinese and English, plus any other system language.
const DICTATION_LANGS = (() => {
  const langs = [["zh-CN", "中文"], ["en-US", "EN"]];
  for (const l of navigator.languages || [navigator.language]) {
    if (l && !/^(zh|en)\b/i.test(l) && !langs.some(([code]) => code === l)) langs.push([l, l.split("-")[0].toUpperCase()]);
  }
  return langs;
})();
function defaultDictationLang() {
  const pick = saved("cmuxDictationLang", "");
  if (DICTATION_LANGS.some(([code]) => code === pick)) return pick;
  const sys = navigator.languages || [navigator.language || "en-US"];
  if (sys.some((l) => /^zh\b/i.test(l))) return "zh-CN";
  const first = DICTATION_LANGS.find(([code]) => code.split("-")[0] === String(sys[0]).split("-")[0].toLowerCase());
  return first ? first[0] : "en-US";
}
// rec: the running recognizer; before/after: the text around the dictated part; original: the text
// before dictation began, for cancel; started: Date.now() at start, for the timer.
// on: the user is dictating; running: the recognizer is between start() and its end event; again: start
// another session when it ends (next utterance, new language); discard: ignore the running session.
// events/error/gotText: what the browser reported during this dictation, shown as a note when it ends
// without text, since engines fail quietly (dictation turned off in Settings, Apple's recognizer failing).
const dictation = { on: false, running: false, again: false, discard: false, lang: "", before: "", after: "", original: "", started: 0, timer: 0,
  events: [], error: "", errorMessage: "", gotText: false, note: "", noteTimer: 0 };

// Each event once in a row, with the seconds since dictation began.
function noteEvent(name) {
  const last = dictation.events[dictation.events.length - 1];
  if (last && last.split(" ")[0] === name) return;
  dictation.events.push(name + " " + ((Date.now() - dictation.started) / 1000).toFixed(1) + "s");
}

// Shown in the bar for a while after a dictation that typed nothing.
function showDictationNote() {
  const hints = {
    "no-speech": "No speech was heard.",
    "audio-capture": "The microphone could not be used; another app may have it.",
    "network": "The speech service could not be reached.",
    "not-allowed": "Microphone access was denied; allow it for this site.",
    "service-not-allowed": "Speech recognition is turned off or unavailable here. On iPhone, turn on Settings › General › Keyboard › Enable Dictation.",
    "language-not-supported": "This browser cannot recognize " + dictation.lang + "; tap the language button next time to pick another.",
  };
  let msg = "Nothing was recognized. " + (hints[dictation.error] || (dictation.error ? "Error: " + dictation.error + "." : ""));
  if (dictation.errorMessage) msg += " (" + dictation.errorMessage + ")";
  dictation.note = msg + " [" + dictation.events.join(" › ") + "]";
  clearTimeout(dictation.noteTimer);
  dictation.noteTimer = setTimeout(() => { dictation.note = ""; renderDictation(); }, 12000);
  renderDictation();
}
// Android Chrome's continuous mode repeats earlier words in later results, reworded or repunctuated so
// that no text comparison can drop them. There a recognizer hears one utterance, and the next starts
// after the caret when it ends, until the user stops or falls silent.
const ONE_UTTERANCE = /Android/i.test(navigator.userAgent);

function dictationBar() {
  let bar = $("dictation");
  if (bar) return bar;
  const lang = el("button", { className: "lang", title: "Recognition language" });
  lang.onclick = () => {
    const i = DICTATION_LANGS.findIndex(([code]) => code === dictation.lang);
    dictation.lang = DICTATION_LANGS[(i + 1) % DICTATION_LANGS.length][0];
    save("cmuxDictationLang", dictation.lang);
    dictation.again = true;
    listen();  // stops the session; the next starts in the new language after what was recognized
  };
  const cancel = el("button", { className: "cancel", title: "Cancel dictation", textContent: "✕" });
  cancel.onclick = () => {
    if (!dictation.on) { dictation.note = ""; renderDictation(); return; }
    const original = dictation.original;
    stopDictation(true);
    $("input").value = original;
    growInput();
  };
  for (const b of [lang, cancel]) b.addEventListener("pointerdown", (e) => { if (document.activeElement === $("input")) e.preventDefault(); });
  const wave = el("span", { className: "wave" });
  for (let i = 0; i < 5; i++) wave.append(el("i"));
  bar = el("div", { id: "dictation", hidden: true },
    el("span", { className: "dot" }), wave,
    el("span", { className: "status", textContent: "Listening…" }),
    el("span", { className: "time", textContent: "0:00" }),
    lang, cancel);
  $("composer").insertBefore(bar, $("composer").querySelector(".box"));
  return bar;
}

function renderDictation() {
  const bar = dictationBar();
  const on = dictation.on;
  bar.hidden = !on && !dictation.note;
  bar.classList.toggle("note", !on && !!dictation.note);
  if (!on && dictation.note) bar.querySelector(".status").textContent = dictation.note;
  $("composer").classList.toggle("dictating", on);
  $("micBtn").classList.toggle("on", on);
  $("micBtn").title = on ? "Stop dictation" : "Dictate";
  clearInterval(dictation.timer);
  if (!on) { bar.classList.remove("hearing"); return; }
  const label = DICTATION_LANGS.find(([code]) => code === dictation.lang);
  bar.querySelector(".lang").textContent = label ? label[1] : dictation.lang;
  const tick = () => {
    const sec = Math.floor((Date.now() - dictation.started) / 1000);
    bar.querySelector(".time").textContent = Math.floor(sec / 60) + ":" + String(sec % 60).padStart(2, "0");
  };
  tick();
  dictation.timer = setInterval(tick, 500);
}

function setHearing(on) {
  const bar = dictationBar();
  bar.classList.toggle("hearing", on);
  bar.querySelector(".status").textContent = on ? "" : "Listening…";
}

// The results of one recognition session, as one text. Each result should be the next stretch of
// speech, but in continuous mode iOS Safari and Android Chrome may repeat earlier words: a result can
// hold everything said so far, or repeat the previous result. So a result that starts with the text so
// far replaces it, one the text already ends with is dropped, and others are appended. Spacing is
// ignored when comparing, as the engines change it between results.
function mergeTranscripts(parts) {
  const bare = (t) => t.replace(/\s+/g, "");
  let text = "";
  for (const part of parts) {
    if (!bare(part)) continue;
    if (bare(part).startsWith(bare(text))) text = part;
    else if (!bare(text).endsWith(bare(part))) text += part;
  }
  return text.replace(/^\s+/, "");
}

// One recognizer for the page, started again for each dictation; a session starts only after the last
// one's end event, never over it.
let recognizer = null;

function getRecognizer() {
  if (recognizer) return recognizer;
  const rec = recognizer = new Speech();
  rec.interimResults = true;
  rec.onresult = (e) => {
    if (dictation.discard) return;
    const input = $("input");
    const text = mergeTranscripts(Array.from(e.results, (r) => r[0].transcript));
    noteEvent("result");
    if (text.trim()) dictation.gotText = true;
    // Words in languages written with spaces need one between them and the text before.
    const gap = text && dictation.before && !/\s$/.test(dictation.before) && !/^\s/.test(text) && !/^(zh|ja|ko)\b/i.test(rec.lang) ? " " : "";
    input.value = dictation.before + gap + text + dictation.after;
    const caret = (dictation.before + gap + text).length;
    input.setSelectionRange(caret, caret);
    growInput();
    setHearing(true);
  };
  rec.onspeechstart = () => { if (!dictation.discard) { noteEvent("speech"); setHearing(true); } };
  rec.onspeechend = () => { if (!dictation.discard) setHearing(false); };
  rec.onstart = () => noteEvent("start");
  rec.onaudiostart = () => noteEvent("audio");
  rec.onsoundstart = () => noteEvent("sound");
  rec.onnomatch = () => noteEvent("nomatch");
  rec.onerror = (e) => {
    if (dictation.discard) return;
    noteEvent("error");
    dictation.error = e.error;
    // Safari reports most failures of Apple's recognizer as "aborted", with the reason only in message.
    dictation.errorMessage = e.message || "";
    dictation.again = false;  // silence ("no-speech") also ends dictation; the note says why
  };
  rec.onend = () => {
    dictation.running = false;
    if (dictation.on && dictation.again) { listen(); return; }
    if (dictation.discard) return;  // cancelled: already shown as off
    noteEvent("end");
    dictation.on = false;
    if (!dictation.gotText) showDictationNote();
    else renderDictation();
  };
  return rec;
}

// Stopping lets the last words arrive; cancelling (on send, cancel or leaving the view) drops them.
function stopDictation(cancel) {
  if (!dictation.on) return;
  dictation.again = false;
  if (cancel) {
    dictation.on = false;
    dictation.discard = true;
    if (dictation.running) recognizer.abort();
    renderDictation();
  } else if (dictation.running) recognizer.stop();
}

// Start a session in dictation.lang that types at the caret, now or when the running one ends.
function listen() {
  if (dictation.running) { dictation.again = true; recognizer.stop(); return; }
  const input = $("input");
  const rec = getRecognizer();
  rec.lang = dictation.lang;
  // Android Chrome repeats earlier words in continuous mode; there each session is one utterance.
  rec.continuous = !ONE_UTTERANCE;
  dictation.again = ONE_UTTERANCE;
  dictation.discard = false;
  dictation.before = input.value.slice(0, input.selectionStart);
  dictation.after = input.value.slice(input.selectionEnd);
  try { rec.start(); } catch (e) {
    dictation.on = false; dictation.error = e.message; showDictationNote(); return;
  }
  dictation.running = true;
  setHearing(false);
  renderDictation();
}

function startDictation() {
  dictation.lang = defaultDictationLang();
  dictation.original = $("input").value;
  dictation.started = Date.now();
  Object.assign(dictation, { on: true, events: [], error: "", errorMessage: "", gotText: false, note: "" });
  listen();
}

// iOS hears only the first dictation after the page loads (WebKit bugs 317741 and 321436), so there
// the button stays hidden and the keyboard's own dictation key, which works, is the way to dictate.
// iPadOS reports itself as a Mac; touch tells them apart.
const IOS = /iPhone|iPad|iPod/.test(navigator.userAgent) || (/Macintosh/.test(navigator.userAgent) && navigator.maxTouchPoints > 1);
$("micBtn").hidden = !Speech || IOS;
$("micBtn").addEventListener("pointerdown", (e) => { if (document.activeElement === $("input")) e.preventDefault(); });
$("micBtn").onclick = () => (dictation.on ? stopDictation(false) : startDictation());

// ---- composer -------------------------------------------------------------------------------
const TERMINAL_KEYS = [["Enter", "enter"], ["Esc", "escape"], ["Ctrl-C", "ctrl+c"], ["Tab", "tab"], ["⇧Tab", "shift+tab"], ["↑", "up"], ["↓", "down"], ["←", "left"], ["→", "right"], ["Ctrl-D", "ctrl+d"], ["PgUp", "pageup"], ["PgDn", "pagedown"]];
const BROWSER_KEYS = [["Enter", "Enter"], ["Tab", "Tab"], ["⌫", "Backspace"], ["Esc", "Escape"], ["↑", "ArrowUp"], ["↓", "ArrowDown"]];
let composerMode = "";

function updateComposer(view, surface) {
  const mode = view === "chat" || view === "terminal" || view === "browser" || view === "new" ? view : "";
  const box = $("composer");
  box.hidden = !mode;
  document.body.classList.toggle("term-mode", mode === "terminal");
  const send = $("sendBtn");
  const chat = mode === "chat" && state.chats[surface.agent.session_id];
  const working = chat && chat.state === "working";
  const stop = working && !$("input").value.trim();
  send.classList.toggle("stop", !!stop);
  send.textContent = stop ? "■" : "↑";
  send.title = stop ? "Stop Claude" : "Send";
  send.classList.toggle("idle", !stop && !$("input").value.trim() && mode !== "new");
  $("attachBtn").hidden = !(mode === "chat" || mode === "terminal" || mode === "new");
  // An ended session takes no messages: its terminal is a shell again.
  const ended = !!chat && chat.state === "ended";
  $("input").disabled = ended;
  if (ended) $("attachBtn").hidden = true;
  box.classList.toggle("ended", ended);
  if (ended) $("input").placeholder = "This session has ended";
  else if (mode === composerMode && mode === "chat") $("input").placeholder = "Reply to Claude";
  if (mode === composerMode) return;
  stopDictation(true);
  composerMode = mode;
  $("input").placeholder = { chat: "Reply to Claude", terminal: "Type a command", browser: "Type into the page", new: "Describe a task (optional)" }[mode] || "";
  $("input").enterKeyHint = mode === "chat" && PHONE.matches ? "enter" : mode === "new" ? "go" : "send";
  const keys = $("keys");
  keys.replaceChildren();
  const list = mode === "terminal" ? TERMINAL_KEYS : mode === "browser" ? BROWSER_KEYS : [];
  for (const [label, key] of list) {
    const b = el("button", { textContent: label });
    b.addEventListener("pointerdown", (e) => { if (document.activeElement === $("input")) e.preventDefault(); });
    b.onclick = () => {
      const { surface: s } = selected();
      if (!s) return;
      if (composerMode === "browser") browserAct({ action: "key", key });
      else api("/api/key", { surface_id: s.id, key }).catch(() => {});
    };
    keys.append(b);
  }
  if (mode === "new") { $("sendBtn").textContent = "↑"; $("sendBtn").classList.remove("stop"); }
}

async function submit() {
  if (uploading) return;
  stopDictation(true);
  const input = $("input");
  const text = input.value;
  const { surface } = selected();
  if (composerMode === "new") {
    input.value = ""; growInput();
    await startSession(text.trim()).catch((e) => alert(e.message));
    return;
  }
  if (!surface) return;
  if (composerMode === "chat") {
    const chat = state.chats[surface.agent.session_id];
    if (!text.trim()) {
      if (chat && chat.state === "working") api("/api/chat/interrupt", { session_id: surface.agent.session_id }).catch(() => {});
      return;
    }
    input.value = ""; growInput();
    await sendChat(surface, text).catch(() => { input.value = text; growInput(); });
  } else if (composerMode === "terminal") {
    input.value = ""; growInput();
    await api("/api/send", { surface_id: surface.id, text, enter: true }).catch(() => { input.value = text; });
  } else if (composerMode === "browser") {
    if (!text) return;
    input.value = ""; growInput();
    await browserAct({ action: "type", text });
  }
  renderMain();
}

function growInput() {
  const box = $("input");
  box.style.height = "auto";
  box.style.height = Math.min(box.scrollHeight, 220) + "px";
  const { surface } = selected();
  if (composerMode) updateComposer(composerMode, surface);
}
$("input").addEventListener("input", growInput);
$("input").addEventListener("keydown", (e) => {
  if (e.key !== "Enter" || e.shiftKey || e.isComposing) return;
  if (composerMode === "chat" && PHONE.matches) return;  // Return adds a line; the button sends
  e.preventDefault();
  submit();
});
// ---- attachments ---------------------------------------------------------------------------
// A photo, screenshot or file goes to the Mac (~/.psycommux/uploads) and its
// path lands in the message, where Claude can read it.
let uploading = 0;
function insertText(text) {
  const input = $("input");
  const at = input.selectionStart ?? input.value.length;
  const end = input.selectionEnd ?? at;
  const before = input.value.slice(0, at);
  const pad = before && !/\s$/.test(before) ? " " : "";
  input.value = before + pad + text + " " + input.value.slice(end);
  const caret = (before + pad + text + " ").length;
  input.setSelectionRange(caret, caret);
  growInput();
}
async function uploadFile(file) {
  const res = await fetch("/api/upload", {
    method: "POST",
    headers: { "Content-Type": file.type || "application/octet-stream", "X-Filename": encodeURIComponent(file.name || "") },
    body: file,
  });
  if (res.status === 401) { location.href = "/"; throw new Error("unauthorized"); }
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.message || data.error || res.statusText);
  return data.path;
}
async function attach(files) {
  const list = [...files].filter((f) => f.size > 0);
  if (!list.length) return;
  const btn = $("attachBtn");
  uploading++;
  btn.classList.add("busy");
  $("sendBtn").disabled = true;
  try {
    for (const file of list) {
      try { insertText(await uploadFile(file)); }
      catch (e) { alert(`Could not send ${file.name || "the file"}: ${e.message}`); }
    }
  } finally {
    if (--uploading === 0) { btn.classList.remove("busy"); $("sendBtn").disabled = false; }
  }
}
$("attachBtn").addEventListener("pointerdown", (e) => { if (document.activeElement === $("input")) e.preventDefault(); });
$("attachBtn").onclick = () => $("attachFile").click();
$("attachFile").onchange = () => { const files = [...$("attachFile").files]; $("attachFile").value = ""; attach(files); };
$("input").addEventListener("paste", (e) => {
  if ($("attachBtn").hidden) return;
  const files = [...(e.clipboardData?.files || [])];
  if (!files.length) return;
  e.preventDefault();
  attach(files);
});
for (const type of ["dragenter", "dragover"]) {
  $("composer").addEventListener(type, (e) => {
    if ($("attachBtn").hidden || !e.dataTransfer?.types.includes("Files")) return;
    e.preventDefault();
    $("composer").classList.add("dropping");
  });
}
$("composer").addEventListener("dragleave", () => $("composer").classList.remove("dropping"));
$("composer").addEventListener("drop", (e) => {
  $("composer").classList.remove("dropping");
  if ($("attachBtn").hidden || !e.dataTransfer?.files.length) return;
  e.preventDefault();
  attach(e.dataTransfer.files);
});

$("sendBtn").addEventListener("pointerdown", (e) => { if (document.activeElement === $("input")) e.preventDefault(); });
$("sendBtn").onclick = submit;

// ---- notifications ----------------------------------------------------------------------------
async function loadNotifications() {
  try {
    const r = await api("/api/notifications");
    state.notifications = r.notifications.slice().sort((a, b) => (b.created_at || "").localeCompare(a.created_at || ""));
    renderNotifications();
    if (state.tree) renderNav();
  } catch (e) { /* the status dot shows connectivity */ }
}

function jumpTo(n) {
  const hit = n.surface_id && findSurface(n.surface_id);
  if (hit) select(hit.ws.id, hit.surface.id);
  else if (n.workspace_id) openWorkspace(n.workspace_id);
}

// "10:43 AM" today, "Yesterday 10:43 AM", then "Nov 16 10:43 AM".
function whenText(d) {
  const clock = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  const day = (x) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const days = Math.round((day(new Date()) - day(d)) / 86400000);
  if (days === 0) return clock;
  if (days === 1) return "Yesterday " + clock;
  return d.toLocaleDateString([], { month: "short", day: "numeric" }) + " " + clock;
}
function renderNotifications() {
  const list = $("nlist");
  list.replaceChildren();
  const unread = state.notifications.filter((n) => !n.is_read).length;
  $("unreadCount").textContent = unread;
  $("unreadCount").hidden = !unread;
  document.title = (unread ? "(" + unread + ") " : "") + "psycommux";
  $("jumpUnread").hidden = $("clearAll").hidden = !state.notifications.length;
  $("jumpUnread").disabled = !unread;
  if (!state.notifications.length) {
    list.append(el("div", { className: "n-empty" }, el("b", { textContent: "No notifications yet" }), "Desktop notifications will appear here for quick review."));
    return;
  }
  for (const n of state.notifications) {
    const clear = el("button", { className: "x", textContent: "✕", title: "Clear notification" });
    clear.onclick = async (e) => {
      e.stopPropagation();
      state.notifications = state.notifications.filter((x) => x.id !== n.id);
      renderNotifications();
      try { await api("/api/notifications/dismiss", { id: n.id }); } catch (err) {}
      loadNotifications();
      scheduleTree();
    };
    const time = n.created_at ? whenText(new Date(n.created_at)) : "";
    const item = el("div", { className: "n" + (n.is_read ? "" : " unread") },
      el("span", { className: "dot" }),
      el("div", { className: "main" },
        el("div", { className: "top" }, el("span", { className: "nt", textContent: n.title || "(no title)" }), el("span", { className: "time", textContent: time })),
        n.body || n.subtitle ? el("div", { className: "nb", textContent: plainMarkdown([n.subtitle, n.body].filter(Boolean).join("\n")).trim() }) : null,
        n.tab_title ? el("div", { className: "tab", textContent: n.tab_title }) : null),
      clear);
    item.onclick = () => { jumpTo(n); document.body.classList.remove("show-notifs"); };
    list.append(item);
  }
}

$("bell").onclick = (e) => { e.stopPropagation(); document.body.classList.toggle("show-notifs"); };
$("notifs").onclick = (e) => e.stopPropagation();
$("jumpUnread").onclick = () => {
  const n = state.notifications.find((x) => !x.is_read);
  if (n) { jumpTo(n); document.body.classList.remove("show-notifs"); }
};
$("clearAll").onclick = async () => {
  state.notifications = [];
  renderNotifications();
  try { await api("/api/notifications/clear_all", {}); } catch (e) {}
  loadNotifications();
  scheduleTree();
};
function showBrowserNotification(n) {
  if (state.pushOn) return;  // the service worker shows it, pushed by the server
  if (!n || !("Notification" in window) || Notification.permission !== "granted") return;
  if (document.visibilityState === "visible" && document.hasFocus()) return;
  const title = n.title || "psycommux";
  const body = [n.subtitle, n.body].filter(Boolean).join("\n");
  try {
    const notice = new Notification(title, { body, tag: n.id });
    notice.onclick = () => { window.focus(); jumpTo(n); };
  } catch (e) {
    // Android's Chrome shows notifications only through a service worker.
    const params = new URLSearchParams();
    if (n.workspace_id) params.set("ws", n.workspace_id);
    if (n.surface_id) params.set("s", n.surface_id);
    const url = "/" + (params.toString() ? "?" + params : "");
    navigator.serviceWorker?.getRegistration()
      .then((reg) => reg && reg.showNotification(title, { body, tag: n.id, icon: "/icon-192.png", data: { url } }))
      .catch(() => {});
  }
}
// Web Push: the server pushes cmux notifications (while the Mac is unattended)
// to this device's service worker, which shows them even with the app closed.
// On iPhone this needs the page added to the Home Screen (iOS 16.4+).
const canPush = "serviceWorker" in navigator && "PushManager" in window && "Notification" in window;
function b64urlBytes(text) {
  const raw = atob(text.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - text.length % 4) % 4));
  return Uint8Array.from(raw, (c) => c.charCodeAt(0));
}
function sameKey(buffer, bytes) {
  if (!buffer) return false;
  const a = new Uint8Array(buffer);
  return a.length === bytes.length && a.every((v, i) => v === bytes[i]);
}
function showPushState(text) {
  $("pushState").textContent = "Notifications on this device: " + text;
  $("enableNotify").textContent = state.pushOn ? "Turn off" : "Enable";
  // Blocked or unsupported: the button could not change anything.
  $("enableNotify").hidden = !state.pushOn && canPush && Notification.permission === "denied" || !canPush;
  $("testPush").hidden = !state.pushOn;
}
// Subscribe (when `ask`, from a tap: it may prompt), or resend an existing
// subscription so the server has it.
async function syncPush(ask) {
  if (!canPush) {
    state.pushOn = false;
    showPushState(/iPhone|iPad/.test(navigator.userAgent) ? "add psycommux to the Home Screen first" : "not supported here");
    return;
  }
  if (Notification.permission === "denied") {
    state.pushOn = false;
    showPushState(/iPhone|iPad/.test(navigator.userAgent) ? "blocked; allow them in Settings → Notifications → psycommux" : "blocked; allow them in the site settings");
    return;
  }
  if (!ask && Notification.permission !== "granted") { state.pushOn = false; showPushState("off"); return; }
  try {
    if (ask && (await Notification.requestPermission()) !== "granted") { showPushState("not allowed"); return; }
    const reg = await navigator.serviceWorker.register("/sw.js");
    await navigator.serviceWorker.ready;
    const key = b64urlBytes((await api("/api/push")).key);
    let sub = await reg.pushManager.getSubscription();
    if (sub && !sameKey(sub.options.applicationServerKey, key)) { await sub.unsubscribe(); sub = null; }
    if (!sub && ask) sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
    if (!sub) { state.pushOn = false; showPushState("off"); return; }
    await api("/api/push/subscribe", { subscription: sub.toJSON() });
    state.pushOn = true;
    showPushState("on");
  } catch (e) {
    state.pushOn = false;
    showPushState("failed (" + e.message + ")");
  }
}
async function pushOff() {
  try {
    const reg = await navigator.serviceWorker.getRegistration();
    const sub = reg && await reg.pushManager.getSubscription();
    if (sub) { await api("/api/push/unsubscribe", { endpoint: sub.endpoint }).catch(() => {}); await sub.unsubscribe(); }
  } catch (e) {}
  state.pushOn = false;
  showPushState("off");
}
$("enableNotify").onclick = () => (state.pushOn ? pushOff() : syncPush(true));
$("testPush").onclick = async () => {
  try {
    const r = await api("/api/push/test", {});
    showPushState(r.sent ? "on, test sent" : "on, but the test failed: " + (r.failed[0] || "no subscription"));
  } catch (e) { showPushState("test failed (" + e.message + ")"); }
};
// A tapped notification opens /?ws=<workspace>&s=<surface>, or messages an open window.
function openFromUrl(url) {
  const params = new URL(url, location.href).searchParams;
  const ws = params.get("ws"), s = params.get("s");
  if (!ws && !s) return;
  if (!state.tree) { state.openAfterLoad = url; return; }
  jumpTo({ workspace_id: ws, surface_id: s });
  document.body.classList.remove("show-notifs");
}
if (canPush) {
  navigator.serviceWorker.addEventListener("message", (e) => { if (e.data && e.data.type === "open") openFromUrl(e.data.url); });
}

// ---- secrets for agents ------------------------------------------------------------------
// `psycommux secret "<label>"` waits for a value from here. The value goes from
// the form straight to the waiting command; the page keeps no copy.
function workspaceTitle(id) {
  const ws = id && findWorkspace(id);
  return ws ? ws.title || "" : "";
}
function renderSecrets() {
  const box = $("secrets");
  box.replaceChildren();
  if (state.secretOpen && !state.secrets.some((r) => r.id === state.secretOpen)) state.secretOpen = "";
  for (const req of state.secrets) {
    const where = workspaceTitle(req.workspace_id);
    box.append(el("button", { className: "secret-req", onclick: () => { state.secretOpen = req.id; renderSecrets(); } },
      el("span", { className: "secret-key", textContent: "🔑" }),
      el("span", { className: "grow" },
        el("b", { textContent: req.label }),
        el("span", { className: "secret-where", textContent: where ? " · " + where : "" })),
      el("span", { className: "secret-go", textContent: "Provide" })));
  }
  const req = state.secrets.find((r) => r.id === state.secretOpen);
  if (req) box.append(secretSheet(req));
}
function secretSheet(req) {
  const close = () => { state.secretOpen = ""; renderSecrets(); };
  const input = el("input", { type: "password", name: "password", autocomplete: "current-password",
    autocapitalize: "off", spellcheck: false, placeholder: "Paste or autofill", required: true });
  input.setAttribute("autocorrect", "off");
  const note = el("div", { className: "secret-note" });
  const show = el("button", { type: "button", className: "ghost", textContent: "Show",
    onclick: () => { const hidden = input.type === "password"; input.type = hidden ? "text" : "password"; show.textContent = hidden ? "Hide" : "Show"; } });
  const send = el("button", { type: "submit", className: "primary", textContent: "Send" });
  const form = el("form", { className: "secret-form", onsubmit: async (e) => {
    e.preventDefault();
    if (!input.value) return;
    send.disabled = true;
    const value = input.value;
    input.value = "";
    try { await api("/api/secrets/answer", { id: req.id, value }); close(); }
    catch (err) { note.textContent = err.message; send.disabled = false; }
  } },
    el("div", { className: "secret-title", textContent: "Secret requested" }),
    el("div", { className: "secret-label", textContent: req.label }),
    workspaceTitle(req.workspace_id) ? el("div", { className: "secret-where", textContent: "from " + workspaceTitle(req.workspace_id) }) : null,
    el("input", { type: "text", name: "username", autocomplete: "username", className: "secret-user", tabIndex: -1, value: req.label }),
    el("div", { className: "secret-row" }, input, show),
    note,
    el("div", { className: "secret-actions" },
      el("button", { type: "button", className: "ghost danger", textContent: "Decline", onclick: async () => {
        try { await api("/api/secrets/decline", { id: req.id }); } catch (err) {}
        close();
      } }),
      el("button", { type: "button", className: "ghost", textContent: "Later", onclick: close }),
      send));
  const sheet = el("div", { className: "secret-sheet", onclick: (e) => { if (e.target === sheet) close(); } }, form);
  setTimeout(() => input.focus(), 50);
  return sheet;
}
function setSecrets(list) {
  // The browser notification comes with the cmux notification the server posts.
  state.secrets = list || [];
  if (state.secretOpen && state.secrets.some((r) => r.id === state.secretOpen)) return;  // keep the open form (and what is typed)
  renderSecrets();
}
async function loadSecrets() {
  try { setSecrets((await api("/api/secrets")).requests); } catch (e) {}
}

// ---- live events -------------------------------------------------------------------------------
function connectEvents() {
  const es = new EventSource("/api/events");
  es.onmessage = (msg) => {
    const m = JSON.parse(msg.data);
    if (m.type === "status") {
      $("status").className = m.connected ? "ok" : "bad";
      $("status").title = m.connected ? "connected to cmux" : "cmux offline";
      if (m.connected) { loadTree(); loadNotifications(); loadSecrets(); }
      return;
    }
    if (m.type === "secrets") { setSecrets(m.requests); return; }
    if (m.type === "sidebar") {
      const ws = findWorkspace(m.workspace_id);
      if (ws) { ws.sidebar = m.sidebar; renderNav(); }
      return;
    }
    if (m.category === "notification") {
      if (m.name === "notification.created") showBrowserNotification(m.notification);
      loadNotifications();
      scheduleTree();
    } else if (m.category === "workspace" || m.category === "surface") {
      scheduleTree();
    }
  };
  es.onerror = () => { $("status").className = "bad"; $("status").title = "reconnecting"; };
}

// ---- shell ---------------------------------------------------------------------------------------
$("menu").onclick = (e) => { e.stopPropagation(); document.body.classList.toggle("show-nav"); };
$("scrim").onclick = () => document.body.classList.remove("show-nav");
$("filter").addEventListener("input", renderNav);
PHONE.addEventListener("change", () => { composerMode = ""; state.mounted = ""; render(); });

// iOS keeps the layout viewport when the keyboard opens; size the page to what is visible.
if (window.visualViewport) {
  const fitViewport = () => {
    document.documentElement.style.setProperty("--vvh", visualViewport.height + "px");
    // The keyboard covers the home indicator: no safe-area gap above it.
    document.body.classList.toggle("kb", window.innerHeight - visualViewport.height > 120);
    if (visualViewport.offsetTop) window.scrollTo(0, 0);
    document.querySelectorAll("#view pre.term, #view .chat-scroll").forEach(restoreScroll);
  };
  visualViewport.addEventListener("resize", fitViewport);
  visualViewport.addEventListener("scroll", fitViewport);
  fitViewport();
}

// ---- agent chat view ----------------------------------------------------------
// Terminals running Claude Code can show their conversation instead of the
// screen: read from the session's transcript, so text wraps to the page rather
// than to the terminal's columns. Input still goes to the same terminal.

function esc(s) {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

// Markdown subset: fences, headings, lists, quotes, tables, rules, inline code,
// bold, italics, http(s) links. Text is escaped before any markup is added.
function mdInline(raw) {
  const codes = [];
  let s = esc(raw).replace(/`([^`\n]+)`/g, (m, c) => { codes.push(c); return `\u0000${codes.length - 1}\u0000`; });
  s = s.replace(/\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
  s = s.replace(/(^|[\s(])(https?:\/\/[^\s<)]+)/g, '$1<a href="$2" target="_blank" rel="noopener">$2</a>');
  // Relative links point into the Mac's filesystem; keep the label only.
  s = s.replace(/\[([^\]\n]+)\]\(([^)\s]+)\)/g, '<span class="rel" title="$2">$1</span>');
  s = s.replace(/\*\*([^*\n]+)\*\*/g, "<b>$1</b>").replace(/(^|[^*\w])\*([^*\n]+)\*(?!\w)/g, "$1<i>$2</i>");
  return s.replace(/\u0000(\d+)\u0000/g, (m, i) => `<code>${codes[+i]}</code>`);
}

function md(text) {
  const lines = text.replace(/\r/g, "").split("\n");
  const out = [];
  const isList = (l) => /^\s*([-*+]|\d+[.)])\s+/.test(l);
  const isBlockStart = (l) => /^\s*(```|#{1,6}\s|>|([-*+]|\d+[.)])\s+|\|)/.test(l);
  let i = 0;
  while (i < lines.length) {
    const l = lines[i];
    if (/^\s*```/.test(l)) {
      const buf = [];
      i++;
      while (i < lines.length && !/^\s*```/.test(lines[i])) buf.push(lines[i++]);
      i++;
      out.push(`<pre class="code">${esc(buf.join("\n"))}</pre>`);
    } else if (/^\s*\|.*\|\s*$/.test(l) && i + 1 < lines.length && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1])) {
      const row = (r) => r.trim().replace(/^\||\|$/g, "").split("|").map((c) => mdInline(c.trim()));
      const head = row(l);
      i += 2;
      const body = [];
      while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) body.push(row(lines[i++]));
      out.push(`<div class="tablewrap"><table><tr>${head.map((c) => `<th>${c}</th>`).join("")}</tr>` +
        body.map((r) => `<tr>${r.map((c) => `<td>${c}</td>`).join("")}</tr>`).join("") + "</table></div>");
    } else if (/^#{1,6}\s/.test(l)) {
      const level = l.match(/^#+/)[0].length;
      out.push(`<div class="h h${Math.min(level, 3)}">${mdInline(l.replace(/^#+\s*/, ""))}</div>`);
      i++;
    } else if (isList(l)) {
      const ordered = /^\s*\d/.test(l);
      const items = [];
      while (i < lines.length && (isList(lines[i]) || (/^\s{2,}\S/.test(lines[i]) && items.length))) {
        const m = lines[i].match(/^(\s*)([-*+]|\d+[.)])\s+(.*)$/);
        if (m) items.push({ indent: m[1].length, text: m[3] });
        else items[items.length - 1].text += " " + lines[i].trim();
        i++;
      }
      const tag = ordered ? "ol" : "ul";
      out.push(`<${tag}>` + items.map((it) => `<li style="margin-left:${Math.min(it.indent, 8) * 0.6}em">${mdInline(it.text)}</li>`).join("") + `</${tag}>`);
    } else if (/^\s*>/.test(l)) {
      const buf = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) buf.push(lines[i++].replace(/^\s*>\s?/, ""));
      out.push(`<blockquote>${md(buf.join("\n"))}</blockquote>`);
    } else if (/^\s*(-{3,}|\*{3,}|_{3,})\s*$/.test(l)) {
      out.push("<hr>");
      i++;
    } else if (!l.trim()) {
      i++;
    } else {
      const buf = [];
      while (i < lines.length && lines[i].trim() && !(buf.length && isBlockStart(lines[i]))) buf.push(lines[i++]);
      out.push(`<p>${buf.map(mdInline).join("<br>")}</p>`);
    }
  }
  return out.join("");
}

const TOOL_ICONS = { Bash: "$", Edit: "✎", MultiEdit: "✎", Write: "✎", NotebookEdit: "✎", Read: "📄", Grep: "🔍", Glob: "🔍",
                     WebFetch: "🌐", WebSearch: "🌐", Task: "◆", Agent: "◆", TodoWrite: "☑", AskUserQuestion: "?" };

function chatState(sid) {
  if (!state.chats[sid]) {
    const scroll = el("div", { className: "chat-scroll" });
    const older = el("button", { className: "chat-older", textContent: "Load earlier messages", hidden: true });
    const msgs = el("div", { className: "msgs" });
    const status = el("div", { className: "chat-status" });
    scroll.append(older, msgs);
    const root = el("div", { className: "chat" }, scroll, status);
    const chat = state.chats[sid] = { sid, root, scroll, older, msgs, status, results: {}, tools: {}, questions: {},
                                      start: 0, end: 0, state: "", loaded: false, peek: "", surfaceId: null };
    scroll.addEventListener("scroll", () => {
      rememberScroll(scroll);
      if (scroll.scrollTop < 80 && chat.start > 0 && !chat.loadingOlder) loadOlder(chat);
    }, { passive: true });
    older.onclick = () => loadOlder(chat);
  }
  return state.chats[sid];
}

function toolRow(item) {
  const row = el("details", { className: "step" });
  row.dataset.call = item.id;
  const icon = TOOL_ICONS[item.tool] || "⚙";
  const name = item.tool.startsWith("mcp__") ? item.tool.split("__").slice(-1)[0] : item.tool;
  const head = el("summary", {},
    el("span", { className: "st" }),
    el("span", { className: "ti", textContent: icon }),
    el("b", { textContent: name }),
    el("span", { className: "ts", textContent: item.tool === "Bash" ? (item.note || item.summary) : item.summary || "" }));
  if (item.additions || item.deletions) {
    head.append(el("span", { className: "diffstat", innerHTML: `<i>+${item.additions}</i> <u>−${item.deletions}</u>` }));
  }
  row.append(head);
  const body = el("div", { className: "sb" });
  if (item.command) body.append(el("pre", { className: "code", textContent: "$ " + item.command }));
  if (item.diff) {
    const pre = el("pre", { className: "code diff" });
    for (const line of item.diff.split("\n")) {
      pre.append(el("span", { className: line[0] === "+" ? "add" : line[0] === "-" ? "del" : "", textContent: line + "\n" }));
    }
    body.append(pre);
  }
  if (item.todos) {
    body.append(el("div", { className: "todos", innerHTML: item.todos.map((t) =>
      `<div class="${t.status}">${t.status === "completed" ? "☑" : t.status === "in_progress" ? "◐" : "☐"} ${esc(t.text)}</div>`).join("") }));
  }
  if (item.detail) body.append(el("pre", { className: "code", textContent: item.detail }));
  body.append(el("pre", { className: "code out", hidden: true }));
  row.append(body);
  return row;
}

function applyResult(chat, result) {
  const row = chat.tools[result.call_id];
  if (row) {
    row.classList.add(result.is_error ? "err" : "ok");
    const out = row.querySelector("pre.out");
    if (result.text) { out.hidden = false; out.textContent = result.text; }
    refreshGroup(row.closest(".steps"));
  }
  for (const card of chat.questions[result.call_id] || []) {
    card.classList.add("answered");
    const ans = card.querySelector(".answer");
    ans.textContent = result.text.replace(/^User has answered[^:]*:\s*/i, "").slice(0, 400);
  }
}

function refreshGroup(group) {
  if (!group) return;
  const rows = group.querySelectorAll(".step");
  const errors = group.querySelectorAll(".step.err").length;
  const running = [...rows].filter((r) => !r.classList.contains("ok") && !r.classList.contains("err")).length;
  const last = rows[rows.length - 1];
  const lastText = last ? last.querySelector("b").textContent + " " + last.querySelector(".ts").textContent : "";
  group.querySelector(":scope > summary .gl").textContent =
    `${rows.length} step${rows.length === 1 ? "" : "s"}` + (errors ? ` · ${errors} failed` : "") + (running ? " · running" : "");
  group.querySelector(":scope > summary .gs").textContent = lastText;
}

function itemNode(chat, item) {
  if (item.kind === "user") return el("div", { className: "msg user", textContent: item.text });
  if (item.kind === "assistant") return el("div", { className: "msg bot", innerHTML: md(item.text) });
  if (item.kind === "note") return el("div", { className: "msg note", textContent: item.text });
  if (item.kind === "question") {
    const card = el("div", { className: "msg question" },
      el("div", { className: "qh", textContent: item.header || "Question" }),
      el("div", { className: "qp", innerHTML: md(item.prompt) }));
    const opts = el("div", { className: "qo" });
    item.options.forEach((o, i) => {
      const b = el("button", {}, el("b", { textContent: `${i + 1}. ${o.label}` }), o.detail ? el("span", { textContent: o.detail }) : null);
      b.onclick = () => answer(chat, i);
      opts.append(b);
    });
    card.append(opts, el("div", { className: "answer" }));
    (chat.questions[item.call_id] = chat.questions[item.call_id] || []).push(card);
    return card;
  }
  return null;
}

// Append items in transcript order; consecutive tool calls share one collapsed group.
function addItems(chat, items, prepend) {
  const frag = document.createDocumentFragment();
  let group = prepend ? null : (chat.msgs.lastElementChild && chat.msgs.lastElementChild.classList.contains("steps") ? chat.msgs.lastElementChild : null);
  const results = [];
  for (const item of items) {
    if (item.kind === "result") { results.push(item); continue; }
    if (item.kind === "tool") {
      if (!group) {
        group = el("details", { className: "steps" }, el("summary", {}, el("span", { className: "gl" }), el("span", { className: "gs" })));
        frag.append(group);
      }
      const row = toolRow(item);
      chat.tools[item.id] = row;
      group.append(row);
      refreshGroup(group);
      continue;
    }
    if (item.kind === "user") chat.msgs.querySelectorAll(".msg.pending").forEach((p) => p.remove());
    const node = itemNode(chat, item);
    if (node) { group = null; frag.append(node); }
  }
  if (prepend) chat.msgs.prepend(frag); else chat.msgs.append(frag);
  for (const r of results) { chat.results[r.call_id] = r; applyResult(chat, r); }
  // Results that arrived before their call (loaded older pages later).
  if (prepend) for (const r of Object.values(chat.results)) applyResult(chat, r);
}

async function loadOlder(chat) {
  chat.loadingOlder = true;
  try {
    const page = await api("/api/chat/history", { session_id: chat.sid, before: chat.start });
    const fromBottom = chat.scroll.scrollHeight - chat.scroll.scrollTop;
    addItems(chat, page.items, true);
    chat.start = page.start;
    chat.older.hidden = chat.start <= 0;
    chat.scroll.scrollTop = chat.scroll.scrollHeight - fromBottom;
  } catch (e) { /* try again on the next scroll */ }
  chat.loadingOlder = false;
}

function setChatState(chat, value) {
  chat.state = value;
  const cur = selected().surface;
  if (cur && cur.agent && cur.agent.session_id === chat.sid) {
    cur.agent.state = value;
    if (viewFor(cur) === "chat") updateComposer("chat", cur);
    renderNav();
  }
  const st = chat.status;
  st.replaceChildren();
  st.className = "chat-status " + value;
  if (value === "working") {
    const stop = el("button", { textContent: "Stop" });
    stop.onclick = () => api("/api/chat/interrupt", { session_id: chat.sid }).catch(() => {});
    st.append(el("span", { className: "spin" }), el("span", { textContent: "Claude is working…" }), stop);
  } else if (value === "needs_input") {
    // Permission prompts are drawn in the terminal and reach the transcript only
    // once answered, so read them off the screen.
    st.append(el("div", { className: "qh", textContent: "Claude needs your input" }));
    renderPrompt(chat);
    refreshPeek(chat);
  } else if (value === "ended") {
    // `claude --resume <id>` in the same terminal, from the session's folder.
    const resume = el("button", { className: "resume", textContent: "Resume session" });
    resume.onclick = async () => {
      resume.disabled = true;
      resume.textContent = "Starting…";
      try {
        await api("/api/chat/resume", { session_id: chat.sid });
        scheduleTree(1500);
      } catch (e) {
        resume.disabled = false;
        resume.textContent = "Resume session";
        failed(e);
      }
    };
    st.append(el("span", { textContent: "This session has ended." }), resume);
  }
}

async function refreshPeek(chat) {
  if (!chat.surfaceId) return;
  try {
    const r = await api("/api/peek", { surface_id: chat.surfaceId });
    const text = (r.text || "").replace(/\s+$/, "");
    if (text === chat.peek) return;
    chat.peek = text;
    if (chat.state === "needs_input") renderPrompt(chat);
  } catch (e) { /* keep the last one */ }
}

// Claude Code's terminal prompt, e.g.
//   │ python -m pytest -q          <- what it wants to run or change
//   ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
//    This command requires approval   <- title
//    Do you want to proceed?          <- question
//    ❯ 1. Yes
//      2. Yes, and don't ask again for …
//    Esc to cancel · Tab to amend
// Returns null when the screen does not end in a numbered menu.
function parsePrompt(text) {
  const raw = text.split("\n").map((l) => l.replace(/\s+$/, ""));
  // A │ bar marks a command drawn as a block; older versions also draw the whole
  // prompt in a box, whose sides go and whose top and bottom act as rules.
  const barred = raw.map((l) => /^\s*│/.test(l));
  const lines = raw.map((l) => l.replace(/^\s*│ ?/, "").replace(/\s*│$/, ""));
  const RULE = /^\s*(?:[─━╌┄-]{8,}|[╭╰].*)$/;
  const OPT = /^\s*(?:[❯›>]\s*)?(\d+)\.\s+(.*)$/;
  let last = -1;
  for (let i = lines.length - 1; i >= 0; i--) if (OPT.test(lines[i])) { last = i; break; }
  if (last < 0) return null;
  let first = last;
  while (first > 0 && (OPT.test(lines[first - 1]) || (/^\s{3,}\S/.test(lines[first - 1]) && !RULE.test(lines[first - 1])))) first--;
  while (first <= last && !OPT.test(lines[first])) first++;
  const options = [];
  for (let i = first; i <= last; i++) {
    const m = lines[i].match(OPT);
    if (m) options.push({ key: m[1], label: m[2].trim() });
    else if (options.length) options[options.length - 1].label += " " + lines[i].trim();
  }
  if (options.length < 2) return null;
  // Text between the menu and the rule above it: the title and the question.
  const head = [];
  let i = first - 1;
  for (; i >= 0 && !RULE.test(lines[i]); i--) if (lines[i].trim()) head.unshift(lines[i]);
  if (!head.length) return null;
  const question = head.pop().trim();
  // Above the rule: what it wants to do, a │ block taken whole, else back to a blank line.
  let context = [];
  const block = i > 0 && barred[i - 1];
  for (let j = i - 1; j >= 0 && context.length < 40 && !RULE.test(lines[j]); j--) {
    if (block ? !barred[j] : context.length && !lines[j].trim()) break;
    if (!context.length && !lines[j].trim()) continue;
    context.unshift(lines[j]);
  }
  // In a box the rule is its top, so the command sits between the title and the question.
  if (!context.length && head.length > 1) context = head.splice(1);
  while (context.length && !context[0].trim()) context.shift();
  const indent = Math.min(...context.filter((l) => l.trim()).map((l) => l.match(/^ */)[0].length));
  return { title: head.map((l) => l.trim()).join(" "), question, options, context: context.map((l) => l.slice(indent)).join("\n") };
}

function renderPrompt(chat) {
  const st = chat.status;
  st.querySelectorAll(":scope > :not(.qh)").forEach((n) => n.remove());
  const send = (text) => chat.surfaceId && api("/api/send", { surface_id: chat.surfaceId, text, enter: false }).then(() => refreshPeek(chat)).catch(failed);
  const esc = () => chat.surfaceId && api("/api/key", { surface_id: chat.surfaceId, key: "escape" }).then(() => refreshPeek(chat)).catch(failed);
  const term = () => { state.viewBySurface[chat.surfaceId] = "terminal"; save("cmuxViews", state.viewBySurface); renderMain(); };
  const p = parsePrompt(chat.peek || "");
  const foot = el("div", { className: "pfoot" }, el("button", { textContent: "Esc", title: "Cancel", onclick: esc }));
  if (!p) {
    foot.append(el("button", { textContent: "Terminal", onclick: term }));
    // Unknown screen: show its bottom and the usual keys.
    const keys = ["1", "2", "3"].map((n) => el("button", { textContent: n, onclick: () => send(n) }));
    foot.prepend(...keys);
    st.append(el("pre", { className: "peek", textContent: (chat.peek || "").split("\n").slice(-14).join("\n") }), foot);
    return;
  }
  if (p.title) st.append(el("div", { className: "ptitle", textContent: p.title }));
  if (p.context) {
    // The command's last lines are what runs; long ones scroll from there.
    const ctx = el("pre", { className: "pctx", textContent: p.context });
    st.append(ctx);
    requestAnimationFrame(() => { ctx.scrollTop = ctx.scrollHeight; });
  }
  st.append(el("div", { className: "pq", textContent: p.question }));
  const opts = el("div", { className: "qo" });
  for (const o of p.options) {
    const b = el("button", { onclick: () => send(o.key) }, el("b", { textContent: o.key }), el("span", { textContent: o.label }));
    if (/^no\b/i.test(o.label)) b.className = "no";
    opts.append(b);
  }
  st.append(opts, foot);
}

function answer(chat, index) {
  if (!chat.surfaceId) return;
  api("/api/send", { surface_id: chat.surfaceId, text: String(index + 1), enter: false }).then(() => refreshPeek(chat)).catch(() => {});
}

async function chatLoop(poll, sid) {
  const chat = chatState(sid);
  while (state.chatPolls[sid] === poll) {
    await untilVisible(poll);
    if (state.chatPolls[sid] !== poll) return;
    try {
      if (!chat.loaded) {
        const page = await api("/api/chat/history", { session_id: sid }, poll.abort.signal);
        chat.msgs.replaceChildren();
        Object.assign(chat, { results: {}, tools: {}, questions: {}, start: page.start, end: page.end, loaded: true });
        addItems(chat, page.items, false);
        chat.older.hidden = chat.start <= 0;
        setChatState(chat, page.state);
        restoreScroll(chat.scroll);
        continue;
      }
      const upd = await api("/api/chat/poll", { session_id: sid, after: chat.end, state: chat.state }, poll.abort.signal);
      if (state.chatPolls[sid] !== poll) return;
      if (upd.reset) { chat.loaded = false; continue; }
      chat.end = upd.end;
      if (upd.items.length) { addItems(chat, upd.items, false); restoreScroll(chat.scroll); }
      if (upd.state !== chat.state) setChatState(chat, upd.state);
      else if (upd.state === "needs_input") refreshPeek(chat);
    } catch (e) {
      if (state.chatPolls[sid] !== poll || e.name === "AbortError") return;
      chat.status.className = "chat-status error";
      chat.status.textContent = "Chat unavailable: " + e.message;
      await new Promise((ok) => setTimeout(ok, 3000));
    }
  }
}

function restartChatPolls() {
  const want = new Set([...document.querySelectorAll("#view .chat")].map((c) => c.dataset.session));
  for (const [sid, poll] of Object.entries(state.chatPolls)) {
    if (!want.has(sid)) { poll.abort.abort(); delete state.chatPolls[sid]; }
  }
  for (const sid of want) {
    if (state.chatPolls[sid]) continue;
    const poll = state.chatPolls[sid] = { abort: new AbortController() };
    chatLoop(poll, sid);
  }
}

async function sendChat(s, text) {
  const chat = chatState(s.agent.session_id);
  const pending = el("div", { className: "msg user pending", textContent: text });
  chat.msgs.append(pending);
  delete state.fromBottom[chat.scroll.dataset.surface];
  restoreScroll(chat.scroll);
  try {
    await api("/api/chat/send", { session_id: s.agent.session_id, text });
  } catch (e) {
    pending.classList.add("failed");
    pending.title = e.message;
    throw e;
  }
}

// ---- terminal painting ------------------------------------------------------
function fitFont(pre, columns) {
  const width = pre.clientWidth - 10;
  if (!columns || width <= 0) return;
  if (PHONE.matches && state.fontMode === "read") pre.style.fontSize = "11px";
  else {
    // Menlo / SF Mono advance is about 0.6em. Phones go smaller so the whole width fits.
    const floor = PHONE.matches ? 4.5 : 7;
    pre.style.fontSize = Math.max(floor, Math.min(13, width / (columns * 0.6))) + "px";
  }
  measureCell(pre);
}
// The real width of one cell at this size (0.6em, give or take rounding), for wide characters.
function measureCell(pre) {
  const size = pre.style.fontSize;
  if (pre.cellFor === size) return;
  const probe = document.createElement("span");
  probe.textContent = "0".repeat(40);
  probe.style.cssText = "position:absolute;visibility:hidden;white-space:pre";
  pre.append(probe);
  const cell = probe.getBoundingClientRect().width / 40;
  probe.remove();
  if (cell > 0) { pre.style.setProperty("--cell", cell + "px"); pre.cellFor = size; }
}

// Scroll position lives in state, not in the <pre>: the tree refresh rebuilds
// every pane, and a fresh <pre> would otherwise start at the top.
function rememberScroll(pre) {
  if (!pre.isConnected || pre.restoring) return;
  const up = pre.scrollHeight - pre.scrollTop - pre.clientHeight;
  if (up < 40) delete state.fromBottom[pre.dataset.surface];
  else state.fromBottom[pre.dataset.surface] = up;
}

function restoreScroll(pre) {
  if (!pre.isConnected) return;
  const s = state.screens[pre.dataset.surface];
  if (s && s.grid) fitFont(pre, s.grid.columns);
  const up = state.fromBottom[pre.dataset.surface] || 0;
  pre.restoring = true;
  pre.scrollTop = Math.max(0, pre.scrollHeight - pre.clientHeight - up);
  requestAnimationFrame(() => { pre.restoring = false; });
}

// Repainting replaces the text, which drops any selection in it. Hold the
// screen still while text is selected there, and catch up once it is let go.
function selectingIn(pre) {
  const s = document.getSelection();
  return !!s && !s.isCollapsed && (pre.contains(s.anchorNode) || pre.contains(s.focusNode));
}
document.addEventListener("selectionchange", () => {
  for (const pre of document.querySelectorAll("pre.term.held")) {
    if (selectingIn(pre)) continue;
    pre.classList.remove("held");
    const screen = state.screens[pre.dataset.surface];
    if (screen) paintScreen(pre, screen);
  }
});

function paintScreen(pre, screen) {
  if (selectingIn(pre)) { pre.classList.add("held"); return; }
  if (screen.grid) paintGrid(pre, screen.grid);
  else { pre.style.color = ""; pre.style.background = ""; pre.textContent = screen.text || (screen.error ? "[" + screen.error + "]" : ""); }
  restoreScroll(pre);
}

// Wide characters (CJK, fullwidth forms, most emoji) take two terminal cells, but the
// font draws them narrower or wider than two Latin cells, so box-drawing tables
// drift out of line. Give each its two cells exactly.
const WIDE = /[\u1100-\u115F\u2E80-\u303E\u3041-\u33FF\u3400-\u4DBF\u4E00-\u9FFF\uA000-\uA4CF\uAC00-\uD7A3\uF900-\uFAFF\uFE30-\uFE4F\uFF00-\uFF60\uFFE0-\uFFE6\u{1F300}-\u{1F64F}\u{1F900}-\u{1F9FF}\u{20000}-\u{3FFFD}]/u;
const WIDE_RUN = new RegExp("(" + WIDE.source + "+)", "u");
function cells(text) {
  if (!WIDE.test(text)) return [text];
  return text.split(WIDE_RUN).filter(Boolean).map((part) => {
    if (!WIDE.test(part)) return part;
    const n = [...part].length;
    const span = document.createElement("span");
    span.className = "w2";
    span.style.width = `calc(var(--cell, 0.6em) * ${2 * n})`;
    span.textContent = part;
    return span;
  });
}

function paintGrid(pre, grid) {
  pre.style.color = grid.foreground;
  pre.style.background = grid.background;
  fitFont(pre, grid.columns);
  const frag = document.createDocumentFragment();
  const cur = grid.cursor || {};
  grid.lines.forEach((spans, lineNo) => {
    let col = 0;
    let pending = "";
    const flush = () => { if (pending) { frag.append(...cells(pending)); pending = ""; } };
    const styled = (text, sid, extraClass) => {
      const css = grid.styles[String(sid)] || "";
      if (!css && !extraClass) { pending += text; return; }
      flush();
      const span = document.createElement("span");
      if (css) span.style.cssText = css;
      if (extraClass) span.className = extraClass;
      span.append(...cells(text));
      frag.append(span);
    };
    const cursorCol = cur.visible && cur.line === lineNo ? cur.column : -1;
    for (const [c, w, text, sid] of spans) {
      if (c > col) { pending += " ".repeat(c - col); col = c; }
      if (cursorCol >= col && cursorCol < c + w) {
        // Split the span around the cursor cell.
        const chars = [...text];
        const cw = chars.length ? w / chars.length : 1;
        const at = Math.min(chars.length - 1, Math.floor((cursorCol - c) / cw));
        if (at > 0) styled(chars.slice(0, at).join(""), sid);
        styled(chars[at] || " ", sid, "cur");
        if (at + 1 < chars.length) styled(chars.slice(at + 1).join(""), sid);
      } else {
        styled(text, sid);
      }
      col = c + w;
    }
    if (cursorCol >= col) { pending += " ".repeat(cursorCol - col); styled(" ", 0, "cur"); }
    pending += "\n";
    flush();
  });
  pre.replaceChildren(frag);
}

// ---- polling every visible terminal in one long-poll -------------------------
function visibleTerminals() {
  return [...document.querySelectorAll("#view pre.term")].map((p) => p.dataset.surface);
}

function restartPolling() {
  const ids = visibleTerminals();
  const key = ids.join(",");
  if (state.poll && state.poll.key === key) return;
  if (state.poll) state.poll.abort.abort();
  if (!ids.length) { state.poll = null; return; }
  state.poll = { key, abort: new AbortController() };
  pollLoop(state.poll, ids);
}

// Agents animate (spinners, timers, a blinking dot), so a busy terminal
// changes every few hundred ms. Redraw at most once a second, and not at all
// while the tab is in the background.
const MIN_POLL_INTERVAL_MS = 1000;

function untilVisible(poll) {
  if (!document.hidden) return Promise.resolve();
  return new Promise((ok) => {
    const done = () => { document.removeEventListener("visibilitychange", check); poll.abort.signal.removeEventListener("abort", done); ok(); };
    const check = () => { if (!document.hidden) done(); };
    document.addEventListener("visibilitychange", check);
    poll.abort.signal.addEventListener("abort", done);
  });
}

async function pollLoop(poll, ids) {
  let lastStart = 0;
  while (state.poll === poll) {
    await untilVisible(poll);
    const wait = lastStart + MIN_POLL_INTERVAL_MS - Date.now();
    if (wait > 0) await new Promise((ok) => setTimeout(ok, wait));
    if (state.poll !== poll) return;
    lastStart = Date.now();
    const surfaces = Object.fromEntries(ids.map((id) => [id, (state.screens[id] || {}).revision || ""]));
    try {
      const r = await api("/api/screens", { surfaces }, poll.abort.signal);
      if (state.poll !== poll) return;
      for (const [id, screen] of Object.entries(r.screens)) {
        if (!screen.changed) continue;
        state.screens[id] = screen;
        document.querySelectorAll(`pre.term[data-surface="${id}"]`).forEach((pre) => paintScreen(pre, screen));
      }
    } catch (e) {
      if (state.poll !== poll || e.name === "AbortError") return;
      await new Promise((ok) => setTimeout(ok, 3000));
    }
  }
}

new ResizeObserver(() => {
  // A new size or font changes the content height; keep the same distance from the bottom.
  for (const pre of document.querySelectorAll("#view pre.term, #view .chat-scroll")) restoreScroll(pre);
}).observe($("view"));


// ---- start ----------------------------------------------------------------------------------------
if (location.search) { state.openAfterLoad = location.href; history.replaceState(null, "", "/"); }
loadTree();
loadNotifications();
connectEvents();
syncPush(false);
