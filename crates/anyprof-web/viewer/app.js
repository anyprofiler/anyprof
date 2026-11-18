"use strict";

// Profile text is written with textContent or canvas fillText. Nothing from
// the file is parsed as HTML.

const ROW = 22;
const PALETTE = [
  [90, 140, 180],
  [70, 128, 148],
  [112, 148, 132],
  [132, 138, 162],
  [78, 118, 168],
  [148, 132, 112],
];

const state = {
  session: null,
  tree: null,
  byParent: new Map(),
  expanded: new Set(),
  flat: [],
  selectedNode: null,
  selectedFrame: null,
  pane: "tree",
  invert: false,
  hideSystem: false,
  query: "",
  sortKey: "weight",
  sortDir: -1,
  tracks: null,
  view: null,
  selection: null,
  generation: 0,
  timeline: null,
};

const $ = (id) => document.getElementById(id);

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

function formatNs(ns) {
  const value = Number(ns);
  if (!Number.isFinite(value)) return "0 ns";
  const sign = value < 0 ? "-" : "";
  const abs = Math.abs(value);
  if (abs >= 1e9) return sign + (abs / 1e9).toFixed(2) + " s";
  if (abs >= 1e6) return sign + (abs / 1e6).toFixed(2) + " ms";
  if (abs >= 1e3) return sign + (abs / 1e3).toFixed(1) + " µs";
  return sign + Math.round(abs) + " ns";
}

function formatCount(value) {
  return Number(value).toLocaleString("en-US");
}

function formatBytes(value) {
  const abs = Math.abs(Number(value));
  if (abs >= 1e9) return (value / 1e9).toFixed(2) + " GB";
  if (abs >= 1e6) return (value / 1e6).toFixed(2) + " MB";
  if (abs >= 1e3) return (value / 1e3).toFixed(1) + " KB";
  return value + " B";
}

function formatWeight(value) {
  const kind = state.session ? state.session.weightKind : "count";
  if (kind === "sampled-time" || kind === "cpu-time" || kind === "duration") {
    return formatNs(value);
  }
  if (kind === "bytes") return formatBytes(value);
  return formatCount(value);
}

function formatPercent(value, total) {
  if (!total) return "0%";
  return ((Number(value) / Number(total)) * 100).toFixed(1) + "%";
}

function weightLabel() {
  switch (state.session.weightKind) {
    case "sampled-time":
      return "sampled time";
    case "cpu-time":
      return "CPU time";
    case "duration":
      return "time";
    case "bytes":
      return "bytes";
    case "other":
      return state.session.unit || "weight";
    default:
      return "samples";
  }
}

function frameOf(id) {
  return state.session.frames[id] || { name: "unknown", system: false };
}

function highlight(text, query) {
  const frag = document.createDocumentFragment();
  if (!query) {
    frag.append(text);
    return frag;
  }
  const lower = text.toLowerCase();
  const needle = query.toLowerCase();
  let index = 0;
  while (index < text.length) {
    const at = lower.indexOf(needle, index);
    if (at < 0) {
      frag.append(text.slice(index));
      break;
    }
    if (at > index) frag.append(text.slice(index, at));
    const mark = document.createElement("mark");
    mark.textContent = text.slice(at, at + needle.length);
    frag.append(mark);
    index = at + needle.length;
  }
  return frag;
}

function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
  try {
    localStorage.setItem("anyprof-theme", theme);
  } catch (_) {
    /* private mode */
  }
  for (const button of document.querySelectorAll(".theme button")) {
    button.setAttribute("aria-pressed", button.dataset.theme === theme ? "true" : "false");
  }
  if (state.pane === "flame") drawFlame();
  if (state.session && state.session.timeline) drawTimeline();
}

function initTheme() {
  let theme = "system";
  try {
    theme = localStorage.getItem("anyprof-theme") || "system";
  } catch (_) {
    theme = "system";
  }
  if (theme !== "light" && theme !== "dark" && theme !== "system") theme = "system";
  applyTheme(theme);
  for (const button of document.querySelectorAll(".theme button")) {
    button.addEventListener("click", () => applyTheme(button.dataset.theme));
  }
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  if (media.addEventListener) {
    media.addEventListener("change", () => {
      if (document.documentElement.dataset.theme === "system") {
        if (state.pane === "flame") drawFlame();
        if (state.session && state.session.timeline) drawTimeline();
      }
    });
  }
}

function showError(message) {
  const overview = $("overview");
  clear(overview);
  const box = el("div", "notice");
  box.dataset.testid = "timeline-status";
  box.append(el("h2", null, "Could not read this profile"));
  box.append(el("p", "error", message));
  overview.append(box);
  const pane = $("pane");
  clear(pane);
  pane.append(el("div", "error", message));
  document.documentElement.dataset.ready = "error";
  document.documentElement.dataset.timeline = "0";
}

function renderMeta() {
  const session = state.session;
  $("file-name").textContent = session.name || "profile";
  document.title = (session.name || "profile") + " — anyprof";
  const parts = [];
  parts.push(session.format);
  parts.push(formatCount(session.samples) + " samples");
  parts.push(weightLabel());
  if (session.wallNs != null) parts.push("wall " + formatNs(session.wallNs));
  if (session.periodNs != null) parts.push("period " + formatNs(session.periodNs));
  if (session.cpuNs != null && session.wallNs) {
    const pct = (Number(session.cpuNs) / Number(session.wallNs)) * 100;
    parts.push("CPU time " + formatNs(session.cpuNs) + " (" + pct.toFixed(0) + "% of wall)");
  }
  $("meta").textContent = parts.join("  ·  ");
  const hide = $("hide-system");
  hide.disabled = !session.hasSystemFrames;
  hide.title = session.hasSystemFrames
    ? "Hide kernel frames and frames mapped from a system library"
    : "No frame is classified as a kernel frame or a system library";
}

function renderOverview() {
  const overview = $("overview");
  clear(overview);
  const session = state.session;
  if (!session.timeline) {
    const box = el("div", "notice");
    box.dataset.testid = "timeline-status";
    box.append(el("h2", null, "No timeline"));
    box.append(el("p", null, session.timelineDisabled || "This profile has no timeline."));
    if (session.notes && session.notes.length) {
      const list = el("ul", "notes");
      for (const note of session.notes) list.append(el("li", null, note));
      box.append(list);
    }
    overview.append(box);
    document.documentElement.dataset.timeline = "0";
    return;
  }
  document.documentElement.dataset.timeline = "1";
  const wrap = el("div", "timeline-wrap");
  wrap.dataset.testid = "timeline-status";
  const head = el("div", "timeline-head");
  head.append(el("strong", null, "Sample activity"));
  const hint = el("span", "hint");
  const kinds = new Set(session.threads.map((thread) => thread.kind));
  if (kinds.size === 1 && kinds.has("profile")) {
    hint.textContent = "Thread ids were not recorded.";
  } else {
    hint.textContent = "Each mark is sample weight in that bucket, not CPU utilization.";
  }
  head.append(hint);
  head.append(el("span", "hint", ""));
  const range = el("span", "hint");
  range.id = "range-label";
  head.append(range);
  const zoom = el("button", null, "Zoom");
  zoom.id = "zoom-range";
  zoom.dataset.testid = "zoom-range";
  zoom.hidden = true;
  zoom.type = "button";
  zoom.addEventListener("click", () => {
    if (!state.selection) return;
    state.view = { start: state.selection.start, end: state.selection.end };
    state.selection = null;
    refreshTimeline();
    refreshTree();
  });
  const reset = el("button", null, "Reset");
  reset.id = "reset-range";
  reset.dataset.testid = "reset-range";
  reset.hidden = true;
  reset.type = "button";
  reset.addEventListener("click", () => {
    state.view = { start: session.timeline.start, end: session.timeline.end };
    state.selection = null;
    refreshTimeline();
    refreshTree();
  });
  head.append(zoom);
  head.append(reset);
  wrap.append(head);
  const ruler = el("div", "ruler");
  ruler.id = "ruler";
  wrap.append(ruler);
  const rows = el("div");
  rows.id = "timeline-rows";
  wrap.append(rows);
  overview.append(wrap);
  renderTrackFilter();
}

function renderTrackFilter() {
  const host = $("tracks");
  clear(host);
  const threads = state.session.threads;
  if (threads.length < 2) return;
  const kinds = new Set(threads.map((thread) => thread.kind));
  const caption = kinds.size === 1 && kinds.has("thread")
    ? "Threads"
    : kinds.size === 1 && kinds.has("profile")
      ? "Profiles"
      : "Tracks";
  host.append(el("span", "muted", caption));
  if (!state.tracks) state.tracks = new Set(threads.map((thread) => thread.id));
  for (const thread of threads) {
    const label = el("label");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = state.tracks.has(thread.id);
    box.addEventListener("change", () => {
      if (box.checked) state.tracks.add(thread.id);
      else state.tracks.delete(thread.id);
      refreshTree();
    });
    label.append(box);
    label.append(thread.name);
    host.append(label);
  }
}

function currentBounds() {
  if (!state.session.timeline) return null;
  return state.selection || state.view;
}

async function refreshTimeline() {
  if (!state.session.timeline) return;
  const view = state.view;
  const width = Math.max(1, ($("timeline-rows") || document.body).clientWidth || 600);
  const buckets = Math.max(20, Math.min(400, Math.floor(width / 3)));
  const params = new URLSearchParams();
  params.set("start", String(view.start));
  params.set("end", String(view.end));
  params.set("buckets", String(buckets));
  const response = await fetch("timeline?" + params.toString());
  const body = await response.json();
  if (!response.ok) {
    state.timeline = null;
    return;
  }
  state.timeline = body;
  drawTimeline();
  const full = state.session.timeline;
  const zoomed = view.start !== full.start || view.end !== full.end;
  const reset = $("reset-range");
  const zoom = $("zoom-range");
  if (reset) reset.hidden = !(zoomed || state.selection);
  if (zoom) zoom.hidden = !state.selection;
  const label = $("range-label");
  if (label) {
    const shown = state.selection || view;
    label.textContent = formatNs(shown.start) + " – " + formatNs(shown.end);
  }
}

function cssColor(name, fallback) {
  const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return value || fallback;
}

function drawTimeline() {
  const host = $("timeline-rows");
  const data = state.timeline;
  if (!host || !data) return;
  const threads = state.session.threads;
  let canvas = $("timeline");
  if (!canvas) {
    clear(host);
    canvas = document.createElement("canvas");
    canvas.id = "timeline";
    canvas.dataset.testid = "timeline";
    canvas.style.display = "block";
    canvas.style.width = "100%";
    host.append(canvas);
    bindTimelineDrag(canvas);
  }
  const width = Math.max(1, host.clientWidth);
  const gutter = 120;
  const plotWidth = Math.max(1, width - gutter);
  const height = Math.max(1, threads.length) * ROW;
  const ratio = window.devicePixelRatio || 1;
  canvas.width = Math.floor(width * ratio);
  canvas.height = Math.floor(height * ratio);
  canvas.style.height = height + "px";
  const ctx = canvas.getContext("2d");
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  const mark = cssColor("--mark", "#7eb6ea");
  const track = cssColor("--track", "#f4f8fc");
  const line = cssColor("--line", "#d8d8dc");
  const ink = cssColor("--muted", "#6e6e73");
  const rows = data.rows || [];
  let max = 1;
  for (const row of rows) for (const value of row) if (Number(value) > max) max = Number(value);
  const span = Math.max(1, Number(data.end) - Number(data.start));
  ctx.font = "11px -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif";
  threads.forEach((thread, index) => {
    const y = index * ROW;
    ctx.fillStyle = track;
    ctx.fillRect(gutter, y, plotWidth, ROW - 1);
    ctx.fillStyle = ink;
    ctx.save();
    ctx.beginPath();
    ctx.rect(0, y, gutter - 6, ROW);
    ctx.clip();
    ctx.fillText(thread.name, 8, y + 15);
    ctx.restore();
    const row = rows[index] || [];
    const bucketWidth = plotWidth / Math.max(1, row.length);
    row.forEach((value, bucket) => {
      if (!value) return;
      const bar = Math.max(1, ((ROW - 6) * Number(value)) / max);
      ctx.fillStyle = mark;
      ctx.fillRect(gutter + bucket * bucketWidth, y + ROW - 3 - bar, Math.max(1, bucketWidth - 0.5), bar);
    });
    ctx.strokeStyle = line;
    ctx.beginPath();
    ctx.moveTo(gutter, y + ROW - 0.5);
    ctx.lineTo(width, y + ROW - 0.5);
    ctx.stroke();
  });
  if (state.selection && span > 0) {
    const x0 = gutter + ((state.selection.start - data.start) / span) * plotWidth;
    const x1 = gutter + ((state.selection.end - data.start) / span) * plotWidth;
    ctx.fillStyle = "rgba(10, 95, 212, 0.18)";
    ctx.fillRect(Math.min(x0, x1), 0, Math.abs(x1 - x0), height);
  }
  paintRuler(data, gutter, plotWidth);
}

function paintRuler(data, gutter, width) {
  const ruler = $("ruler");
  if (!ruler) return;
  clear(ruler);
  ruler.style.marginLeft = gutter + "px";
  const start = Number(data.start);
  const end = Number(data.end);
  const span = Math.max(1, end - start);
  const ticks = 4;
  for (let index = 0; index <= ticks; index += 1) {
    const tick = el("span", null, formatNs(start + (span * index) / ticks));
    tick.style.left = (width * index) / ticks + "px";
    ruler.append(tick);
  }
}

function bindTimelineDrag(canvas) {
  let origin = null;
  let dragged = false;
  let dragging = false;
  const tickAt = (event) => {
    const data = state.timeline;
    if (!data) return 0;
    const rect = canvas.getBoundingClientRect();
    const gutter = 120;
    const plot = Math.max(1, rect.width - gutter);
    const ratio = Math.min(1, Math.max(0, (event.clientX - rect.left - gutter) / plot));
    const span = Math.max(1, Number(data.end) - Number(data.start));
    return Math.round(Number(data.start) + ratio * span);
  };
  canvas.addEventListener("pointerdown", (event) => {
    if (event.button != null && event.button !== 0) return;
    origin = tickAt(event);
    dragged = false;
    dragging = true;
    try {
      canvas.setPointerCapture(event.pointerId);
    } catch (_) {
      /* capture is optional; moves over the canvas still select */
    }
  });
  canvas.addEventListener("pointermove", (event) => {
    if (!dragging) return;
    const next = tickAt(event);
    if (next !== origin) dragged = true;
    const start = Math.min(origin, next);
    const end = Math.max(origin, next);
    if (end > start) state.selection = { start, end };
    drawTimeline();
  });
  const finish = () => {
    if (!dragging) return;
    dragging = false;
    origin = null;
    if (!dragged) state.selection = null;
    updateRangeControls();
    refreshTree();
  };
  canvas.addEventListener("pointerup", finish);
  canvas.addEventListener("pointercancel", finish);
}

function updateRangeControls() {
  const zoom = $("zoom-range");
  const reset = $("reset-range");
  const label = $("range-label");
  const full = state.session && state.session.timeline;
  if (!full) return;
  const zoomed = state.view.start !== full.start || state.view.end !== full.end;
  if (zoom) zoom.hidden = !state.selection;
  if (reset) reset.hidden = !(zoomed || state.selection);
  if (label) {
    const shown = state.selection || state.view;
    label.textContent = formatNs(shown.start) + " – " + formatNs(shown.end);
  }
}

function treeParams() {
  const params = new URLSearchParams();
  if (state.invert) params.set("invert", "1");
  if (state.hideSystem) params.set("hideSystem", "1");
  const bounds = currentBounds();
  if (bounds) {
    params.set("start", String(bounds.start));
    params.set("end", String(bounds.end));
  }
  if (state.tracks && state.session.threads.length > 1) {
    params.set("threads", [...state.tracks].join(","));
  }
  return params;
}

async function refreshTree() {
  const generation = ++state.generation;
  const response = await fetch("tree?" + treeParams().toString());
  const body = await response.json();
  if (generation !== state.generation) return;
  if (!response.ok) {
    showError(body.error || "Could not build the call tree");
    return;
  }
  state.tree = body;
  indexTree();
  if (state.expanded.size === 0) expandDefaults();
  renderPane();
  renderInspector();
  document.documentElement.dataset.ready = "1";
}

function indexTree() {
  state.byParent = new Map();
  for (const node of state.tree.nodes) {
    const key = node.parent == null ? "root" : node.parent;
    if (!state.byParent.has(key)) state.byParent.set(key, []);
    state.byParent.get(key).push(node);
  }
}

function expandDefaults() {
  const nodes = state.tree.nodes;
  if (nodes.length <= 400) {
    for (const node of nodes) {
      if ((state.byParent.get(node.id) || []).length) state.expanded.add(node.id);
    }
    return;
  }
  for (const node of nodes) {
    if (node.depth < 2 && (state.byParent.get(node.id) || []).length) state.expanded.add(node.id);
  }
}

function compareNodes(a, b) {
  const dir = state.sortDir;
  if (state.sortKey === "name") {
    const an = frameOf(a.frame).name;
    const bn = frameOf(b.frame).name;
    return an < bn ? -dir : an > bn ? dir : a.id - b.id;
  }
  if (state.sortKey === "self") return (a.self - b.self) * dir || b.weight - a.weight || a.id - b.id;
  return (a.weight - b.weight) * dir || (b.self - a.self) || a.id - b.id;
}

function kidsOf(key) {
  const list = (state.byParent.get(key) || []).slice();
  list.sort(compareNodes);
  return list;
}

function nameMatches(frameId) {
  if (!state.query) return true;
  return frameOf(frameId).name.toLowerCase().includes(state.query.toLowerCase());
}

function flatten() {
  const show = state.query ? matchingIds() : null;
  const out = [];
  const walk = (node) => {
    if (show && !show.has(node.id)) return;
    out.push(node);
    const open = state.query || state.expanded.has(node.id);
    if (open) for (const kid of kidsOf(node.id)) walk(kid);
  };
  for (const root of kidsOf("root")) walk(root);
  state.flat = out;
  return out;
}

function matchingIds() {
  const show = new Set();
  const walk = (node) => {
    let hit = nameMatches(node.frame);
    for (const kid of state.byParent.get(node.id) || []) if (walk(kid)) hit = true;
    if (hit) show.add(node.id);
    return hit;
  };
  for (const root of state.byParent.get("root") || []) walk(root);
  return show;
}

function renderPane() {
  if (state.pane === "flame") renderFlame();
  else if (state.pane === "functions") renderFunctions();
  else renderTree();
}

function renderTree() {
  const pane = $("pane");
  const previous = pane.querySelector(".scroll");
  const scrollTop = previous ? previous.scrollTop : 0;
  clear(pane);
  if (!state.tree.nodes.length || !flatten().length) {
    pane.append(el("div", "empty", state.query ? "No functions match." : "No samples in this range."));
    return;
  }
  const table = el("div", "table");
  table.append(columnHeader());
  const scroll = el("div", "scroll");
  scroll.dataset.testid = "tree-scroll";
  const top = el("div");
  const rows = el("div");
  rows.dataset.testid = "tree-rows";
  const bottom = el("div");
  scroll.append(top, rows, bottom);
  table.append(scroll);
  pane.append(table);
  const paint = () => {
    const flat = flatten();
    const height = scroll.clientHeight || 400;
    const start = Math.max(0, Math.floor(scroll.scrollTop / ROW) - 4);
    const count = Math.ceil(height / ROW) + 12;
    const slice = flat.slice(start, start + count);
    top.style.height = start * ROW + "px";
    bottom.style.height = Math.max(0, flat.length - (start + slice.length)) * ROW + "px";
    clear(rows);
    for (const node of slice) rows.append(treeRow(node));
  };
  scroll.addEventListener("scroll", paint);
  paint();
  scroll.scrollTop = scrollTop;
}

function columnHeader() {
  const header = el("div", "cols");
  const name = el("button", null, "Symbol");
  name.type = "button";
  name.addEventListener("click", () => setSort("name"));
  const weight = el("button", "num", "Weight");
  weight.type = "button";
  weight.addEventListener("click", () => setSort("weight"));
  const self = el("button", "num", "Self");
  self.type = "button";
  self.addEventListener("click", () => setSort("self"));
  header.append(name, weight, self, el("span", "num", "%"));
  return header;
}

function setSort(key) {
  if (state.sortKey === key) state.sortDir *= -1;
  else {
    state.sortKey = key;
    state.sortDir = key === "name" ? 1 : -1;
  }
  renderPane();
}

function treeRow(node) {
  const row = el("div", "row");
  row.dataset.testid = "tree-row";
  row.dataset.node = String(node.id);
  row.setAttribute("role", "treeitem");
  row.setAttribute("aria-selected", node.id === state.selectedNode ? "true" : "false");
  const children = state.byParent.get(node.id) || [];
  const name = el("div", "name-cell");
  name.style.paddingLeft = 6 + node.depth * 14 + "px";
  if (children.length) {
    const twist = el("button", "twist" + (state.expanded.has(node.id) || state.query ? " open" : ""));
    twist.type = "button";
    twist.setAttribute("aria-label", state.expanded.has(node.id) ? "Collapse" : "Expand");
    twist.addEventListener("click", (event) => {
      event.stopPropagation();
      if (state.expanded.has(node.id)) state.expanded.delete(node.id);
      else state.expanded.add(node.id);
      renderTree();
    });
    name.append(twist);
  } else {
    name.append(el("span", "twist-gap"));
  }
  const frame = frameOf(node.frame);
  const label = el("span", "label");
  label.append(highlight(frame.name, state.query));
  if (frame.runtime && frame.runtime !== "native") {
    label.append(" ");
    label.append(el("span", "muted", frame.runtime));
  }
  name.append(label);
  row.append(name);
  row.append(el("span", "num", formatWeight(node.weight)));
  row.append(el("span", "num", formatWeight(node.self)));
  row.append(el("span", "num", formatPercent(node.weight, state.tree.total)));
  row.addEventListener("click", () => selectNode(node));
  return row;
}

function selectNode(node) {
  state.selectedNode = node.id;
  state.selectedFrame = node.frame;
  for (const row of document.querySelectorAll(".row, .fn-row, .inspector li")) {
    const selected = row.dataset.node === String(node.id) || row.dataset.frame === String(node.frame);
    row.setAttribute("aria-selected", selected && (row.dataset.node === String(node.id) || !row.dataset.node) ? "true" : "false");
  }
  renderInspector();
  if (state.pane === "flame") drawFlame();
}

function renderFunctions() {
  const pane = $("pane");
  clear(pane);
  const rows = state.tree.functions.slice();
  const query = state.query.toLowerCase();
  const filtered = query
    ? rows.filter((row) => frameOf(row.frame).name.toLowerCase().includes(query))
    : rows;
  filtered.sort((a, b) => {
    if (state.sortKey === "name") {
      const an = frameOf(a.frame).name;
      const bn = frameOf(b.frame).name;
      return an < bn ? -state.sortDir : an > bn ? state.sortDir : 0;
    }
    if (state.sortKey === "self") return (a.self - b.self) * state.sortDir;
    return (a.weight - b.weight) * state.sortDir;
  });
  if (!filtered.length) {
    pane.append(el("div", "empty", "No functions match."));
    return;
  }
  const table = el("div", "table");
  table.append(columnHeader());
  const scroll = el("div", "scroll");
  const top = el("div");
  const body = el("div");
  body.dataset.testid = "function-rows";
  const bottom = el("div");
  scroll.append(top, body, bottom);
  table.append(scroll);
  pane.append(table);
  const paint = () => {
    const height = scroll.clientHeight || 400;
    const start = Math.max(0, Math.floor(scroll.scrollTop / ROW) - 4);
    const count = Math.ceil(height / ROW) + 12;
    const slice = filtered.slice(start, start + count);
    top.style.height = start * ROW + "px";
    bottom.style.height = Math.max(0, filtered.length - (start + slice.length)) * ROW + "px";
    clear(body);
    for (const row of slice) {
      const line = el("div", "fn-row row");
      line.dataset.testid = "function-row";
      line.dataset.frame = String(row.frame);
      line.setAttribute("aria-selected", row.frame === state.selectedFrame ? "true" : "false");
      const name = el("div", "name-cell");
      name.style.paddingLeft = "8px";
      const label = el("span", "label");
      label.append(highlight(frameOf(row.frame).name, state.query));
      name.append(label);
      line.append(name);
      line.append(el("span", "num", formatWeight(row.weight)));
      line.append(el("span", "num", formatWeight(row.self)));
      line.append(el("span", "num", formatPercent(row.weight, state.tree.total)));
      line.addEventListener("click", () => {
        state.selectedFrame = row.frame;
        state.selectedNode = null;
        renderFunctions();
        renderInspector();
      });
      body.append(line);
    }
  };
  scroll.addEventListener("scroll", paint);
  paint();
}

function renderFlame() {
  const pane = $("pane");
  clear(pane);
  if (!state.tree.nodes.length) {
    pane.append(el("div", "empty", "No samples in this range."));
    return;
  }
  if (state.query && matchingIds().size === 0) {
    pane.append(el("div", "empty", "No functions match."));
    return;
  }
  const host = el("div", "flame-host");
  const canvas = document.createElement("canvas");
  canvas.id = "flame";
  canvas.dataset.testid = "flame";
  host.append(canvas);
  const tip = el("div", "tip");
  tip.hidden = true;
  host.append(tip);
  pane.append(host);
  drawFlame();
}

function flameRects(rowH, pixelWidth) {
  const rects = [];
  const minimum = 0.5 / Math.max(1, pixelWidth);
  const walk = (node, x, y, width) => {
    rects.push({ node, x, y, width, height: Math.max(8, rowH - 1) });
    let cursor = x;
    const kids = kidsOf(node.id).filter((kid) => !state.query || matchingIdsCached(kid.id));
    for (const kid of kids) {
      const childWidth = node.weight ? (width * kid.weight) / node.weight : 0;
      if (childWidth >= minimum) walk(kid, cursor, y + rowH, childWidth);
      cursor += childWidth;
    }
  };
  const show = state.query ? matchingIds() : null;
  matchingIdsCached = (id) => !show || show.has(id);
  const roots = kidsOf("root").filter((node) => matchingIdsCached(node.id));
  const total = roots.reduce((sum, node) => sum + node.weight, 0) || 1;
  let cursor = 0;
  for (const root of roots) {
    const width = root.weight / total;
    walk(root, cursor, 0, width);
    cursor += width;
  }
  return rects;
}

let matchingIdsCached = () => true;

function drawFlame() {
  const canvas = $("flame");
  if (!canvas) return;
  const host = canvas.parentElement;
  const width = Math.max(1, host.clientWidth);
  const levels = state.tree.nodes.reduce((max, node) => Math.max(max, node.depth), 0) + 1;
  const rowH = Math.max(18, Math.floor((Math.max(host.clientHeight, 120) - 4) / levels));
  const rects = flameRects(rowH, width);
  const height = Math.max(host.clientHeight, levels * rowH);
  const ratio = window.devicePixelRatio || 1;
  canvas.width = Math.floor(width * ratio);
  canvas.height = Math.floor(height * ratio);
  canvas.style.width = width + "px";
  canvas.style.height = height + "px";
  const ctx = canvas.getContext("2d");
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  ctx.clearRect(0, 0, width, height);
  const text = cssColor("--text", "#1c1c1e");
  for (const rect of rects) {
    const pixel = rect.width * width;
    if (pixel < 0.5) continue;
    const frame = frameOf(rect.node.frame);
    const color = PALETTE[hash(frame.name) % PALETTE.length];
    const selected = rect.node.frame === state.selectedFrame;
    const barH = Math.max(8, rect.height - 1);
    ctx.fillStyle = selected
      ? cssColor("--select", "#0a5fd4")
      : "rgb(" + color[0] + "," + color[1] + "," + color[2] + ")";
    ctx.fillRect(rect.x * width + 0.5, rect.y + 0.5, Math.max(0, pixel - 1), barH);
    if (pixel > 36) {
      ctx.fillStyle = selected ? "#fff" : text;
      ctx.font = "11px -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif";
      ctx.save();
      ctx.beginPath();
      ctx.rect(rect.x * width + 2, rect.y, Math.max(0, pixel - 4), barH);
      ctx.clip();
      ctx.fillText(frame.name, rect.x * width + 4, rect.y + Math.min(barH - 4, 13));
      ctx.restore();
    }
  }
  const tip = host.querySelector(".tip");
  canvas.onmousemove = (event) => {
    const hit = hitFlame(rects, event, width);
    if (!hit) {
      tip.hidden = true;
      return;
    }
    const frame = frameOf(hit.node.frame);
    tip.hidden = false;
    tip.textContent = frame.name + "  " + formatWeight(hit.node.weight) + "  " + formatPercent(hit.node.weight, state.tree.total);
    tip.style.left = Math.min(event.clientX + 12, window.innerWidth - 440) + "px";
    tip.style.top = event.clientY + 14 + "px";
  };
  canvas.onmouseleave = () => {
    tip.hidden = true;
  };
  canvas.onclick = (event) => {
    const hit = hitFlame(rects, event, width);
    if (!hit) return;
    state.selectedNode = hit.node.id;
    state.selectedFrame = hit.node.frame;
    drawFlame();
    renderInspector();
  };
}

function hitFlame(rects, event, width) {
  const rect = event.currentTarget.getBoundingClientRect();
  const x = (event.clientX - rect.left) / width;
  const y = event.clientY - rect.top;
  for (let index = rects.length - 1; index >= 0; index -= 1) {
    const item = rects[index];
    if (x >= item.x && x < item.x + item.width && y >= item.y && y < item.y + item.height) return item;
  }
  return null;
}

function hash(text) {
  let value = 0;
  for (let index = 0; index < text.length; index += 1) value = (value * 33 + text.charCodeAt(index)) >>> 0;
  return value;
}

function renderInspector() {
  const heavy = $("heaviest");
  clear(heavy);
  const frames = state.tree ? state.tree.heaviest : [];
  if (!frames.length) {
    heavy.append(el("li", "muted", "No stack"));
  } else {
    frames.forEach((frameId, index) => {
      const item = el("li", null, frameOf(frameId).name);
      item.dataset.frame = String(frameId);
      item.setAttribute("aria-selected", frameId === state.selectedFrame ? "true" : "false");
      item.addEventListener("click", () => {
        state.selectedFrame = frameId;
        const node = state.tree.nodes.find((candidate) => candidate.frame === frameId);
        state.selectedNode = node ? node.id : null;
        renderPane();
        renderInspector();
      });
      item.prepend(el("span", "muted", index === 0 ? "" : ""));
      heavy.append(item);
    });
  }
  const detail = $("detail");
  clear(detail);
  if (state.selectedFrame == null) {
    detail.append(el("p", "muted", "Select a function."));
    return;
  }
  const frame = frameOf(state.selectedFrame);
  const row = (state.tree.functions || []).find((item) => item.frame === state.selectedFrame);
  const node = state.tree.nodes.find((item) => item.id === state.selectedNode);
  addDetail(detail, "Name", frame.name);
  addDetail(detail, "Runtime", frame.runtime || "native");
  if (frame.file) {
    let where = frame.file;
    if (frame.line) where += ":" + frame.line;
    if (frame.column) where += ":" + frame.column;
    addDetail(detail, "Source", where);
  }
  if (frame.module) addDetail(detail, "Module", frame.module);
  addDetail(detail, "System library", frame.system ? "yes" : "no");
  const weight = row ? row.weight : node ? node.weight : 0;
  const self = row ? row.self : node ? node.self : 0;
  addDetail(detail, "Weight", formatWeight(weight));
  addDetail(detail, "Self", formatWeight(self));
  addDetail(detail, "Share", formatPercent(weight, state.tree.total));
}

function addDetail(detail, name, value) {
  detail.append(el("dt", null, name));
  detail.append(el("dd", null, value));
}

function setPane(name) {
  state.pane = name;
  for (const button of document.querySelectorAll(".tabs button")) {
    button.setAttribute("aria-selected", button.dataset.pane === name ? "true" : "false");
  }
  renderPane();
}

function onKey(event) {
  const typing = event.target && (event.target.id === "search" || event.target.tagName === "INPUT");
  if (event.key === "/" && !typing) {
    event.preventDefault();
    $("search").focus();
    return;
  }
  if (event.key === "Escape") {
    if (state.query) {
      state.query = "";
      $("search").value = "";
      renderPane();
    } else if (state.selection || (state.view && state.session.timeline && (state.view.start !== state.session.timeline.start || state.view.end !== state.session.timeline.end))) {
      state.selection = null;
      state.view = { start: state.session.timeline.start, end: state.session.timeline.end };
      refreshTimeline();
      refreshTree();
    }
    return;
  }
  if (typing) return;
  if (event.key === "i" && !event.metaKey && !event.ctrlKey) {
    event.preventDefault();
    $("invert").click();
    return;
  }
  if (state.pane !== "tree") return;
  const flat = state.flat;
  const index = flat.findIndex((node) => node.id === state.selectedNode);
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    const next = event.key === "ArrowDown" ? Math.min(flat.length - 1, index + 1) : Math.max(0, index < 0 ? 0 : index - 1);
    if (flat[next]) {
      selectNode(flat[next]);
      const row = document.querySelector('.row[data-node="' + flat[next].id + '"]');
      if (row) row.scrollIntoView({ block: "nearest" });
      else renderTree();
    }
  } else if (event.key === "ArrowRight" && index >= 0) {
    event.preventDefault();
    const node = flat[index];
    if ((state.byParent.get(node.id) || []).length && !state.expanded.has(node.id)) {
      state.expanded.add(node.id);
      renderTree();
    }
  } else if (event.key === "ArrowLeft" && index >= 0) {
    event.preventDefault();
    const node = flat[index];
    if (state.expanded.has(node.id)) {
      state.expanded.delete(node.id);
      renderTree();
    } else if (node.parent != null) {
      const parent = state.tree.nodes.find((candidate) => candidate.id === node.parent);
      if (parent) selectNode(parent);
    }
  }
}

async function main() {
  initTheme();
  document.addEventListener("keydown", onKey);
  for (const button of document.querySelectorAll(".tabs button")) {
    button.addEventListener("click", () => setPane(button.dataset.pane));
  }
  $("search").addEventListener("input", () => {
    state.query = $("search").value.trim();
    renderPane();
  });
  $("invert").addEventListener("click", () => {
    state.invert = !state.invert;
    $("invert").setAttribute("aria-pressed", state.invert ? "true" : "false");
    state.expanded.clear();
    refreshTree();
  });
  $("hide-system").addEventListener("click", () => {
    if ($("hide-system").disabled) return;
    state.hideSystem = !state.hideSystem;
    $("hide-system").setAttribute("aria-pressed", state.hideSystem ? "true" : "false");
    state.expanded.clear();
    refreshTree();
  });
  window.addEventListener("resize", () => {
    if (state.pane === "flame") drawFlame();
    if (state.session && state.session.timeline) drawTimeline();
  });
  let session;
  try {
    const response = await fetch("session");
    session = await response.json();
    if (!response.ok) {
      showError(session.error || "Could not read this profile");
      return;
    }
  } catch (err) {
    showError(String(err && err.message ? err.message : err));
    return;
  }
  state.session = session;
  if (session.timeline) {
    state.view = { start: session.timeline.start, end: session.timeline.end };
  }
  renderMeta();
  renderOverview();
  if (session.timeline) await refreshTimeline();
  await refreshTree();
}

main();
