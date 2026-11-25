"use strict";

// Profile text and local source lines are written strictly via textContent or
// canvas fillText. Nothing from the profile or source file is parsed as HTML.

const ROW = 21;
const TRACK = 26;
const GUTTER = 128;
const FLAME_ROW = 18;
const FLAME_LEVELS = 512;

const FLAME_LIGHT = {
  node: [[238, 188, 104], [232, 174, 88], [242, 198, 122], [224, 164, 78]],
  browser: [[238, 188, 104], [232, 174, 88], [242, 198, 122], [224, 164, 78]],
  python: [[118, 202, 176], [102, 190, 164], [134, 210, 186], [92, 178, 152]],
  rust: [[232, 154, 122], [222, 138, 106], [240, 168, 138], [212, 128, 96]],
  go: [[120, 194, 228], [104, 182, 220], [138, 204, 234], [94, 170, 210]],
  java: [[178, 156, 226], [164, 140, 218], [190, 170, 232], [152, 128, 208]],
  kernel: [[234, 140, 132], [224, 124, 116], [240, 156, 148], [214, 114, 106]],
  native: [[144, 182, 224], [130, 170, 216], [158, 192, 230], [118, 158, 206]],
  meta: [[172, 184, 204], [162, 176, 198]],
};

const FLAME_DARK = {
  node: [[166, 116, 38], [152, 104, 32], [178, 126, 46], [140, 94, 28]],
  browser: [[166, 116, 38], [152, 104, 32], [178, 126, 46], [140, 94, 28]],
  python: [[38, 128, 106], [32, 116, 96], [46, 140, 116], [28, 104, 86]],
  rust: [[164, 84, 54], [150, 74, 46], [176, 94, 62], [138, 66, 40]],
  go: [[40, 118, 162], [34, 106, 148], [48, 130, 174], [30, 96, 136]],
  java: [[108, 82, 168], [96, 72, 156], [120, 92, 180], [88, 64, 144]],
  kernel: [[164, 68, 64], [150, 58, 54], [176, 78, 72], [138, 50, 48]],
  native: [[54, 102, 158], [46, 92, 146], [64, 114, 170], [40, 82, 134]],
  meta: [[68, 84, 112], [60, 76, 104]],
};

const state = {
  session: null,
  tree: null,
  byId: new Map(),
  byParent: new Map(),
  expanded: new Set(),
  flat: [],
  selectedNode: null,
  selectedFrame: null,
  zoomNode: null,
  pane: "tree",
  previousPane: "tree",
  invert: false,
  hideSystem: false,
  query: "",
  sortKey: "weight",
  sortDir: -1,
  tracks: null,
  view: null,
  selection: null,
  generation: 0,
  sourceGen: 0,
  timeline: null,
  flameHeight: null,
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

function isDark() {
  const mode = document.documentElement.dataset.theme || "system";
  if (mode === "dark") return true;
  if (mode === "light") return false;
  return !!(window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches);
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

function timeScale(start, end) {
  const span = Math.max(1, Number(end) - Number(start));
  const step = span / 4;
  const units = [
    { size: 1e9, suffix: "s" },
    { size: 1e6, suffix: "ms" },
    { size: 1e3, suffix: "µs" },
    { size: 1, suffix: "ns" },
  ];
  for (const unit of units) {
    if (step >= unit.size) return unit;
  }
  return units[units.length - 1];
}

function formatTick(ns, unit) {
  const scaled = Number(ns) / unit.size;
  const rounded = Math.round(scaled * 1000) / 1000;
  let text;
  if (Math.abs(rounded - Math.round(rounded)) < 1e-6) text = String(Math.round(rounded));
  else if (Math.abs(rounded * 10 - Math.round(rounded * 10)) < 1e-4) text = rounded.toFixed(1);
  else text = rounded.toFixed(2);
  if (text === "-0") text = "0";
  return text + " " + unit.suffix;
}

function formatRange(start, end) {
  const unit = timeScale(start, end);
  return formatTick(start, unit) + " – " + formatTick(end, unit);
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
    if (!Number(value)) return "0";
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

function shortFile(raw) {
  if (!raw) return "";
  if (raw.startsWith("node:") || raw.startsWith("wasm:") || raw.startsWith("<")) return raw;
  const normalized = raw.replace(/^file:\/\/(localhost)?/, "").replace(/\\/g, "/");
  const parts = normalized.split("/").filter(Boolean);
  return parts.length ? parts[parts.length - 1] : raw;
}

function formatLocationShort(frame) {
  if (frame.file) {
    let loc = shortFile(frame.file);
    if (frame.line) loc += ":" + frame.line;
    return loc;
  }
  if (frame.module) return shortFile(frame.module);
  return "";
}

function formatLocationFull(frame) {
  if (frame.file) {
    let loc = frame.file.replace(/^file:\/\/(localhost)?/, "");
    if (frame.line) loc += ":" + frame.line;
    if (frame.column) loc += ":" + frame.column;
    return loc;
  }
  if (frame.module) return frame.module;
  return "";
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
  box.append(el("strong", null, "Could not read this profile"));
  const copy = el("div", "copy");
  copy.append(el("p", "error", message));
  box.append(copy);
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
    box.append(el("strong", null, "No timeline"));
    const copy = el("div", "copy");
    copy.append(el("p", null, session.timelineDisabled || "This profile has no timeline."));
    if (session.notes && session.notes.length) {
      const list = el("ul", "notes");
      for (const note of session.notes) list.append(el("li", null, note));
      copy.append(list);
    }
    box.append(copy);
    overview.append(box);
    document.documentElement.dataset.timeline = "0";
    renderTrackFilter();
    return;
  }
  document.documentElement.dataset.timeline = "1";
  const wrap = el("div", "timeline-wrap");
  wrap.dataset.testid = "timeline-status";
  const head = el("div", "timeline-head");
  head.append(el("strong", null, "Sample activity"));
  const kinds = new Set(session.threads.map((thread) => thread.kind));
  const profileTracks = kinds.size === 1 && kinds.has("profile");
  const hint = el("span", "hint");
  hint.textContent = profileTracks
    ? "Not CPU utilization. Profiles, not thread ids."
    : "Not CPU utilization.";
  head.append(hint);
  const help = el("button", "mini", "Details");
  help.type = "button";
  help.id = "timeline-help";
  help.dataset.testid = "timeline-help";
  help.setAttribute("aria-expanded", "false");
  help.setAttribute("aria-controls", "timeline-help-panel");
  head.append(help);
  const range = el("span", "range");
  range.id = "range-label";
  head.append(range);
  const zoom = el("button", "mini", "Zoom");
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
  const reset = el("button", "mini", "Reset");
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
  const panel = el("div", "help-panel");
  panel.id = "timeline-help-panel";
  panel.dataset.testid = "timeline-help-panel";
  panel.hidden = true;
  const paragraphs = [
    "Each mark is sample weight in a time bucket. Spacing and darkness follow that weight. This is not a CPU percent.",
  ];
  if (profileTracks) paragraphs.push("Thread ids were not recorded.");
  if (session.notes) {
    for (const note of session.notes) paragraphs.push(note);
  }
  for (const text of paragraphs) panel.append(el("p", null, text));
  help.addEventListener("click", () => {
    panel.hidden = !panel.hidden;
    help.setAttribute("aria-expanded", panel.hidden ? "false" : "true");
  });
  wrap.append(panel);
  const ruler = el("div", "ruler");
  ruler.id = "ruler";
  wrap.append(ruler);
  const rows = el("div");
  rows.id = "timeline-rows";
  wrap.append(rows);
  const tip = el("div", "tip");
  tip.id = "timeline-tip";
  tip.hidden = true;
  wrap.append(tip);
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
  const group = el("div", "segment");
  group.setAttribute("role", "group");
  group.setAttribute("aria-label", caption);
  for (const thread of threads) {
    const button = el("button", null, thread.name);
    button.type = "button";
    button.title = thread.name;
    button.setAttribute("aria-pressed", state.tracks.has(thread.id) ? "true" : "false");
    button.addEventListener("click", () => {
      if (state.tracks.has(thread.id) && state.tracks.size > 1) state.tracks.delete(thread.id);
      else if (state.tracks.has(thread.id)) {
        for (const t of threads) state.tracks.add(t.id);
      } else {
        state.tracks.add(thread.id);
      }
      for (const [idx, btn] of [...group.querySelectorAll("button")].entries()) {
        btn.setAttribute("aria-pressed", state.tracks.has(threads[idx].id) ? "true" : "false");
      }
      refreshTree();
    });
    group.append(button);
  }
  host.append(group);
}

function currentBounds() {
  if (!state.session.timeline) return null;
  return state.selection || state.view;
}

async function refreshTimeline() {
  if (!state.session.timeline) return;
  const view = state.view;
  const width = Math.max(1, ($("timeline-rows") || document.body).clientWidth || 600);
  const plot = Math.max(1, width - GUTTER);
  const buckets = Math.max(16, Math.min(140, Math.floor(plot / 8)));
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
  updateRangeControls();
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
  const gutter = GUTTER;
  const plotWidth = Math.max(1, width - gutter);
  const height = Math.max(1, threads.length) * TRACK;
  const ratio = window.devicePixelRatio || 1;
  canvas.width = Math.floor(width * ratio);
  canvas.height = Math.floor(height * ratio);
  canvas.style.height = height + "px";
  const ctx = canvas.getContext("2d");
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  const mark = cssColor("--mark", "#4c8fd6");
  const track = cssColor("--track", "#f3f6fa");
  const line = cssColor("--line", "#d2d2d7");
  const ink = cssColor("--muted", "#5c5c63");
  const rows = data.rows || [];
  let max = 1;
  for (const row of rows) for (const value of row) if (Number(value) > max) max = Number(value);
  const span = Math.max(1, Number(data.end) - Number(data.start));
  ctx.font = "11px -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif";
  threads.forEach((thread, index) => {
    const y = index * TRACK;
    ctx.fillStyle = track;
    ctx.fillRect(gutter, y, plotWidth, TRACK - 1);
    ctx.fillStyle = ink;
    ctx.save();
    ctx.beginPath();
    ctx.rect(0, y, gutter - 6, TRACK);
    ctx.clip();
    ctx.fillText(thread.name, 8, y + 17);
    ctx.restore();
  });
  paintSelection(ctx, data, gutter, plotWidth, height, span, false);
  threads.forEach((_, index) => {
    const y = index * TRACK;
    const row = rows[index] || [];
    const bucketWidth = plotWidth / Math.max(1, row.length);
    // Keep each mark narrower than its slot so a uniform series stays a crisp comb.
    const markWidth = Math.min(2.5, Math.max(1, bucketWidth * 0.34));
    const maxBar = TRACK - 6;
    row.forEach((value, bucket) => {
      if (!value) return;
      const share = Math.max(0, Math.min(1, Number(value) / max));
      const bar = 3 + (maxBar - 3) * share;
      ctx.save();
      ctx.globalAlpha = 0.48 + 0.52 * share;
      ctx.fillStyle = mark;
      ctx.fillRect(
        gutter + bucket * bucketWidth + (bucketWidth - markWidth) / 2,
        y + TRACK - 3 - bar,
        markWidth,
        bar
      );
      ctx.restore();
    });
    ctx.strokeStyle = line;
    ctx.beginPath();
    ctx.moveTo(gutter, y + TRACK - 0.5);
    ctx.lineTo(width, y + TRACK - 0.5);
    ctx.stroke();
  });
  ctx.strokeStyle = line;
  ctx.beginPath();
  ctx.moveTo(gutter - 0.5, 0);
  ctx.lineTo(gutter - 0.5, height);
  ctx.stroke();
  paintSelection(ctx, data, gutter, plotWidth, height, span, true);
  paintRuler(data, gutter, plotWidth);
}

function paintSelection(ctx, data, gutter, plotWidth, height, span, edges) {
  if (!state.selection || span <= 0) return;
  const x0 = gutter + ((state.selection.start - data.start) / span) * plotWidth;
  const x1 = gutter + ((state.selection.end - data.start) / span) * plotWidth;
  const left = Math.max(gutter, Math.min(x0, x1));
  const right = Math.min(gutter + plotWidth, Math.max(x0, x1));
  const wide = Math.max(1, right - left);
  ctx.save();
  ctx.beginPath();
  ctx.rect(gutter, 0, plotWidth, height);
  ctx.clip();
  if (!edges) {
    ctx.fillStyle = "rgba(10, 95, 212, 0.10)";
    ctx.fillRect(left, 0, wide, height);
  } else {
    ctx.strokeStyle = "rgba(10, 95, 212, 0.9)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(left + 0.5, 0);
    ctx.lineTo(left + 0.5, height);
    ctx.moveTo(Math.max(left + 1, right) - 0.5, 0);
    ctx.lineTo(Math.max(left + 1, right) - 0.5, height);
    ctx.stroke();
  }
  ctx.restore();
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
  const unit = timeScale(start, end);
  for (let index = 0; index <= ticks; index += 1) {
    const tick = el("span", null, formatTick(start + (span * index) / ticks, unit));
    tick.style.left = (width * index) / ticks + "px";
    if (index === 0) tick.style.transform = "translateX(0)";
    else if (index === ticks) tick.style.transform = "translateX(-100%)";
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
    const gutter = GUTTER;
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
    const tip = $("timeline-tip");
    if (tip) tip.hidden = true;
    try {
      canvas.setPointerCapture(event.pointerId);
    } catch (_) {
      /* capture is optional */
    }
  });
  canvas.addEventListener("pointermove", (event) => {
    const data = state.timeline;
    if (!data) return;
    if (dragging) {
      const next = tickAt(event);
      if (next !== origin) dragged = true;
      const start = Math.min(origin, next);
      const end = Math.max(origin, next);
      if (end > start) state.selection = { start, end };
      drawTimeline();
      return;
    }
    const tip = $("timeline-tip");
    if (!tip) return;
    const rect = canvas.getBoundingClientRect();
    const x = event.clientX - rect.left - GUTTER;
    const y = event.clientY - rect.top;
    const plot = Math.max(1, rect.width - GUTTER);
    const rowIdx = Math.floor(y / TRACK);
    const row = (data.rows || [])[rowIdx];
    if (x < 0 || x > plot || !row || !row.length) {
      tip.hidden = true;
      return;
    }
    const bucket = Math.min(row.length - 1, Math.max(0, Math.floor((x / plot) * row.length)));
    const span = Math.max(1, Number(data.end) - Number(data.start));
    const bStart = Math.round(Number(data.start) + (span * bucket) / row.length);
    const bEnd = Math.round(Number(data.start) + (span * (bucket + 1)) / row.length);
    const thread = state.session.threads[rowIdx];
    tip.hidden = false;
    tip.textContent = (thread ? thread.name + "  ·  " : "") + formatRange(bStart, bEnd) + "  ·  " + formatWeight(row[bucket]);
    tip.style.left = Math.min(event.clientX + 12, window.innerWidth - 360) + "px";
    tip.style.top = event.clientY + 14 + "px";
  });
  canvas.addEventListener("pointerleave", () => {
    const tip = $("timeline-tip");
    if (tip) tip.hidden = true;
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
    label.textContent = formatRange(shown.start, shown.end);
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
  if (state.zoomNode != null && !state.byId.has(state.zoomNode)) {
    state.zoomNode = null;
  }
  if (!state.byId.has(state.selectedNode)) {
    state.selectedNode = null;
    state.selectedFrame = null;
    const roots = kidsOf("root");
    if (roots[0]) {
      state.selectedNode = roots[0].id;
      state.selectedFrame = roots[0].frame;
    }
  }
  if (state.expanded.size === 0) expandDefaults();
  renderPane();
  renderInspector();
  document.documentElement.dataset.ready = "1";
}

function indexTree() {
  state.byId = new Map();
  state.byParent = new Map();
  for (const node of state.tree.nodes) {
    state.byId.set(node.id, node);
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
  if (state.sortKey === "loc") {
    const al = formatLocationShort(frameOf(a.frame));
    const bl = formatLocationShort(frameOf(b.frame));
    return al < bl ? -dir : al > bl ? dir : b.weight - a.weight || a.id - b.id;
  }
  if (state.sortKey === "self") return (a.self - b.self) * dir || b.weight - a.weight || a.id - b.id;
  return (a.weight - b.weight) * dir || (b.self - a.self) || a.id - b.id;
}

function kidsOf(key) {
  const list = (state.byParent.get(key) || []).slice();
  list.sort(compareNodes);
  return list;
}

function frameMatches(frameId) {
  if (!state.query) return true;
  const needle = state.query.toLowerCase();
  const frame = frameOf(frameId);
  if (frame.name.toLowerCase().includes(needle)) return true;
  if (frame.file && frame.file.toLowerCase().includes(needle)) return true;
  if (frame.module && frame.module.toLowerCase().includes(needle)) return true;
  return false;
}

function flatten() {
  const show = state.query ? matchingIds() : null;
  const out = [];
  const pending = [];
  const roots = kidsOf("root");
  for (let index = roots.length - 1; index >= 0; index -= 1) pending.push(roots[index]);
  while (pending.length) {
    const node = pending.pop();
    if (show && !show.has(node.id)) continue;
    out.push(node);
    if (!(state.query || state.expanded.has(node.id))) continue;
    const kids = kidsOf(node.id);
    for (let index = kids.length - 1; index >= 0; index -= 1) pending.push(kids[index]);
  }
  state.flat = out;
  return out;
}

function matchingIds() {
  const show = new Set();
  const pending = [];
  for (const root of state.byParent.get("root") || []) pending.push({ node: root, seen: false });
  while (pending.length) {
    const item = pending.pop();
    const kids = state.byParent.get(item.node.id) || [];
    if (!item.seen) {
      pending.push({ node: item.node, seen: true });
      for (let index = kids.length - 1; index >= 0; index -= 1) {
        pending.push({ node: kids[index], seen: false });
      }
      continue;
    }
    let hit = frameMatches(item.node.frame);
    for (const kid of kids) if (show.has(kid.id)) hit = true;
    if (hit) show.add(item.node.id);
  }
  return show;
}

function renderPane() {
  if (state.pane === "flame") renderFlame();
  else if (state.pane === "functions") renderFunctions();
  else if (state.pane === "source") renderSource();
  else renderTree();
}

function openSourceForFrame(frameId, nodeId) {
  if (frameId == null) return;
  state.selectedFrame = frameId;
  if (nodeId != null) state.selectedNode = nodeId;
  if (state.pane !== "source") state.previousPane = state.pane;
  setPane("source");
  renderInspector();
}

function renderTree(host) {
  const pane = host || $("pane");
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

function headerLabel(key, text) {
  if (state.sortKey !== key) return text;
  return text + (state.sortDir < 0 ? " ↓" : " ↑");
}

function columnHeader() {
  const header = el("div", "cols");
  const name = el("button", "name-head", headerLabel("name", "Symbol"));
  name.type = "button";
  name.addEventListener("click", () => setSort("name"));
  const loc = el("button", "loc-head", headerLabel("loc", "Location"));
  loc.type = "button";
  loc.addEventListener("click", () => setSort("loc"));
  const weight = el("button", null, headerLabel("weight", "Weight"));
  weight.type = "button";
  weight.addEventListener("click", () => setSort("weight"));
  const self = el("button", null, headerLabel("self", "Self"));
  self.type = "button";
  self.addEventListener("click", () => setSort("self"));
  const pct = el("button", null, "%");
  pct.type = "button";
  pct.addEventListener("click", () => setSort("weight"));
  const selfPct = el("button", null, "Self %");
  selfPct.type = "button";
  selfPct.addEventListener("click", () => setSort("self"));
  header.append(name, loc, weight, self, pct, selfPct);
  return header;
}

function setSort(key) {
  if (state.sortKey === key) state.sortDir *= -1;
  else {
    state.sortKey = key;
    state.sortDir = (key === "name" || key === "loc") ? 1 : -1;
  }
  renderPane();
}

function metricCell(value, total, barClass) {
  const cell = el("span", "num");
  const share = total > 0 ? Math.min(100, Math.max(0, (Number(value) / Number(total)) * 100)) : 0;
  if (share >= 0.5) {
    const bar = el("span", "cell-bar " + barClass);
    bar.style.width = share.toFixed(1) + "%";
    cell.append(bar);
  }
  cell.append(document.createTextNode(formatWeight(value)));
  return cell;
}

function locationCell(frame, frameId, nodeId) {
  const short = formatLocationShort(frame);
  const full = formatLocationFull(frame);
  const cell = el("span", "loc-cell" + (frame.file ? " loc-link" : ""));
  if (short) {
    cell.append(highlight(short, state.query));
    cell.title = frame.file ? full + " (click to view source)" : full;
  }
  if (frame.file) {
    cell.addEventListener("click", (event) => {
      event.stopPropagation();
      openSourceForFrame(frameId, nodeId);
    });
  }
  return cell;
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
      if (state.pane === "flame") {
        const host = document.querySelector(".flame-tree");
        if (host) renderTree(host);
      } else {
        renderTree();
      }
    });
    name.append(twist);
  } else {
    name.append(el("span", "twist-gap"));
  }
  const frame = frameOf(node.frame);
  const label = el("span", "label");
  label.append(highlight(frame.name, state.query));
  name.append(label);
  if (frame.runtime && frame.runtime !== "native") {
    name.append(el("span", "rt-badge", frame.runtime));
  }
  row.append(
    name,
    locationCell(frame, node.frame, node.id),
    metricCell(node.weight, state.tree.total, "cell-bar-weight"),
    metricCell(node.self, state.tree.total, "cell-bar-self"),
    el("span", "num", formatPercent(node.weight, state.tree.total)),
    el("span", "num", formatPercent(node.self, state.tree.total))
  );
  row.addEventListener("click", () => selectNode(node));
  row.addEventListener("dblclick", () => {
    if (frame.file) openSourceForFrame(node.frame, node.id);
  });
  return row;
}

function selectNode(node) {
  state.selectedNode = node.id;
  state.selectedFrame = node.frame;
  for (const row of document.querySelectorAll(".row, .fn-row, .inspector li")) {
    const selected = row.dataset.node === String(node.id) || (!row.dataset.node && row.dataset.frame === String(node.frame));
    row.setAttribute("aria-selected", selected ? "true" : "false");
  }
  renderInspector();
  if (state.pane === "flame") drawFlame();
}

function focusNodeInTree(node) {
  state.selectedNode = node.id;
  state.selectedFrame = node.frame;
  let cur = node.parent != null ? state.byId.get(node.parent) : null;
  const seen = new Set();
  while (cur && !seen.has(cur.id)) {
    seen.add(cur.id);
    state.expanded.add(cur.id);
    cur = cur.parent != null ? state.byId.get(cur.parent) : null;
  }
  const treeHost = document.querySelector(".flame-tree");
  if (treeHost) {
    renderTree(treeHost);
    const flat = state.flat;
    const idx = flat.findIndex((item) => item.id === node.id);
    const scroller = treeHost.querySelector(".scroll");
    if (scroller && idx >= 0) {
      const targetTop = idx * ROW;
      if (targetTop < scroller.scrollTop || targetTop + ROW > scroller.scrollTop + scroller.clientHeight) {
        scroller.scrollTop = Math.max(0, targetTop - Math.floor(scroller.clientHeight / 3));
      }
    }
  }
  renderInspector();
}

function renderFunctions() {
  const pane = $("pane");
  clear(pane);
  const rows = state.tree.functions.slice();
  const filtered = state.query
    ? rows.filter((row) => frameMatches(row.frame))
    : rows;
  filtered.sort((a, b) => {
    if (state.sortKey === "name") {
      const an = frameOf(a.frame).name;
      const bn = frameOf(b.frame).name;
      return an < bn ? -state.sortDir : an > bn ? state.sortDir : 0;
    }
    if (state.sortKey === "loc") {
      const al = formatLocationShort(frameOf(a.frame));
      const bl = formatLocationShort(frameOf(b.frame));
      return al < bl ? -state.sortDir : al > bl ? state.sortDir : b.weight - a.weight;
    }
    if (state.sortKey === "self") return (a.self - b.self) * state.sortDir || b.weight - a.weight;
    return (a.weight - b.weight) * state.sortDir || b.self - a.self;
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
      const frame = frameOf(row.frame);
      const line = el("div", "fn-row row");
      line.dataset.testid = "function-row";
      line.dataset.frame = String(row.frame);
      line.setAttribute("aria-selected", row.frame === state.selectedFrame ? "true" : "false");
      const name = el("div", "name-cell");
      name.style.paddingLeft = "8px";
      const label = el("span", "label");
      label.append(highlight(frame.name, state.query));
      name.append(label);
      if (frame.runtime && frame.runtime !== "native") {
        name.append(el("span", "rt-badge", frame.runtime));
      }
      line.append(
        name,
        locationCell(frame, row.frame, null),
        metricCell(row.weight, state.tree.total, "cell-bar-weight"),
        metricCell(row.self, state.tree.total, "cell-bar-self"),
        el("span", "num", formatPercent(row.weight, state.tree.total)),
        el("span", "num", formatPercent(row.self, state.tree.total))
      );
      line.addEventListener("click", () => {
        state.selectedFrame = row.frame;
        const candidate = state.tree.nodes.find((item) => item.frame === row.frame);
        state.selectedNode = candidate ? candidate.id : null;
        renderFunctions();
        renderInspector();
      });
      line.addEventListener("dblclick", () => {
        if (frame.file) openSourceForFrame(row.frame, null);
      });
      body.append(line);
    }
  };
  scroll.addEventListener("scroll", paint);
  paint();
}

async function renderSource() {
  const pane = $("pane");
  clear(pane);
  const wrap = el("div", "source-pane");
  wrap.dataset.testid = "source-view";
  pane.append(wrap);

  if (state.selectedFrame == null) {
    wrap.append(el("div", "empty", "Select a function in Call Tree, Flame Graph, or Functions to inspect its source."));
    return;
  }

  const head = el("div", "source-head");
  head.dataset.testid = "source-header";
  const titleWrap = el("div", "source-title-wrap");
  const frame = frameOf(state.selectedFrame);
  const short = formatLocationShort(frame) || frame.name;
  const full = formatLocationFull(frame);
  titleWrap.append(el("div", "source-title", frame.name + (short ? "  ·  " + short : "")));
  if (full) titleWrap.append(el("div", "source-path", full));
  head.append(titleWrap);

  const back = el("button", "mini", "← Back");
  back.type = "button";
  back.dataset.testid = "source-back";
  back.addEventListener("click", () => setPane(state.previousPane || "tree"));
  head.append(back);
  wrap.append(head);

  const gen = ++state.sourceGen;
  const params = treeParams();
  params.set("frame", String(state.selectedFrame));
  let body;
  try {
    const resp = await fetch("source?" + params.toString());
    body = await resp.json();
    if (gen !== state.sourceGen || state.pane !== "source") return;
    if (!resp.ok) {
      wrap.append(el("div", "error", body.error || "Could not load source."));
      return;
    }
  } catch (err) {
    if (gen !== state.sourceGen) return;
    wrap.append(el("div", "error", String(err && err.message ? err.message : err)));
    return;
  }

  if (!body.available) {
    const card = el("div", "source-unavailable");
    card.dataset.testid = "source-unavailable";
    card.append(el("h3", null, full || frame.name));
    card.append(el("p", null, body.reason || "Source file is not available on local disk."));
    if (body.hotLines && body.hotLines.length) {
      const hotWrap = el("div", "source-hot-list");
      hotWrap.append(el("p", null, "Recorded sample weights by line in this file:"));
      for (const item of body.hotLines) {
        hotWrap.append(el(
          "div",
          "source-path",
          "Line " + item.line + "  ·  Weight " + formatWeight(item.weight) + "  ·  Self " + formatWeight(item.self)
        ));
      }
      card.append(hotWrap);
    }
    wrap.append(card);
    return;
  }

  const cols = el("div", "source-cols");
  cols.append(
    el("span", null, "Line"),
    el("span", null, "Weight"),
    el("span", null, "Self"),
    el("span", null, "Source")
  );
  wrap.append(cols);

  const scroll = el("div", "source-scroll");
  scroll.dataset.testid = "source-rows";
  let targetEl = null;
  const total = state.tree ? state.tree.total : 0;

  for (const item of body.lines || []) {
    const classes = ["source-row"];
    if (item.weight > 0 || item.self > 0) classes.push("source-hot");
    if (body.line && item.line === body.line) classes.push("source-target");
    const row = el("div", classes.join(" "));
    row.dataset.testid = "source-line";
    row.dataset.line = String(item.line);

    const ln = el("span", "source-ln", String(item.line));
    const wCell = el("span", "source-metric");
    if (item.weight > 0) {
      const pct = total > 0 ? Math.min(100, (item.weight / total) * 100) : 0;
      if (pct >= 0.5) {
        const bar = el("span", "cell-bar cell-bar-weight");
        bar.style.width = pct.toFixed(1) + "%";
        wCell.append(bar);
      }
      wCell.append(document.createTextNode(formatWeight(item.weight)));
    }
    const sCell = el("span", "source-metric");
    if (item.self > 0) {
      const pct = total > 0 ? Math.min(100, (item.self / total) * 100) : 0;
      if (pct >= 0.5) {
        const bar = el("span", "cell-bar cell-bar-self");
        bar.style.width = pct.toFixed(1) + "%";
        sCell.append(bar);
      }
      sCell.append(document.createTextNode(formatWeight(item.self)));
    }
    const code = el("span", "source-code");
    code.append(highlight(item.text, state.query));
    row.append(ln, wCell, sCell, code);
    scroll.append(row);
    if (body.line && item.line === body.line) targetEl = row;
  }
  wrap.append(scroll);
  if (targetEl) {
    requestAnimationFrame(() => {
      targetEl.scrollIntoView({ block: "center" });
    });
  }
}

function zoomAncestors(nodeId) {
  const chain = [];
  const seen = new Set();
  let cur = state.byId.get(nodeId);
  while (cur && !seen.has(cur.id)) {
    seen.add(cur.id);
    chain.push(cur);
    cur = cur.parent != null ? state.byId.get(cur.parent) : null;
  }
  chain.reverse();
  return chain;
}

function renderFlameZoomBar(bar) {
  clear(bar);
  const crumbs = el("div", "flame-crumbs");
  if (state.zoomNode == null || !state.byId.has(state.zoomNode)) {
    crumbs.append(el("span", "muted", "Click any bar to zoom into its subtree  ·  Esc resets"));
    bar.append(crumbs);
    return;
  }
  const allBtn = el("button", "flame-crumb", "All");
  allBtn.type = "button";
  allBtn.addEventListener("click", () => {
    state.zoomNode = null;
    updateFlameView();
  });
  crumbs.append(allBtn);

  const chain = zoomAncestors(state.zoomNode);
  for (let i = 0; i < chain.length; i += 1) {
    const node = chain[i];
    const last = i === chain.length - 1;
    crumbs.append(el("span", "flame-sep", "›"));
    const label = frameOf(node.frame).name + (last ? " (" + formatPercent(node.weight, state.tree.total) + ")" : "");
    const btn = el("button", "flame-crumb" + (last ? " active" : ""), label);
    btn.type = "button";
    btn.addEventListener("click", () => {
      state.zoomNode = last ? null : node.id;
      focusNodeInTree(node);
      updateFlameView();
    });
    crumbs.append(btn);
  }
  bar.append(crumbs);

  const reset = el("button", "mini", "Reset zoom");
  reset.type = "button";
  reset.dataset.testid = "flame-reset";
  reset.addEventListener("click", () => {
    state.zoomNode = null;
    updateFlameView();
  });
  bar.append(reset);
}

function updateFlameView() {
  const bar = $("flame-zoom-bar");
  if (bar) renderFlameZoomBar(bar);
  drawFlame();
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
  const column = el("div", "flame-column");
  const zoomBar = el("div", "flame-bar");
  zoomBar.id = "flame-zoom-bar";
  zoomBar.dataset.testid = "flame-zoom-bar";
  renderFlameZoomBar(zoomBar);

  const band = el("div", "flame-band");
  band.id = "flame-band";
  if (state.flameHeight != null) {
    band.style.height = state.flameHeight + "px";
    band.style.maxHeight = state.flameHeight + "px";
  }
  const host = el("div", "flame-host");
  const canvas = document.createElement("canvas");
  canvas.id = "flame";
  canvas.dataset.testid = "flame";
  host.append(canvas);
  const tip = el("div", "tip");
  tip.hidden = true;
  host.append(tip);
  band.append(host);

  const levels = state.tree.nodes.reduce((max, node) => Math.max(max, node.depth), 0) + 1;
  if (levels > FLAME_LEVELS) {
    band.append(el(
      "p",
      "flame-note",
      "The flame graph shows the first " + FLAME_LEVELS + " of " + levels + " levels. The call tree lists every frame, and weights are unchanged."
    ));
  }

  const splitter = el("div", "splitter splitter-y");
  splitter.id = "split-flame";
  splitter.dataset.testid = "split-flame";
  splitter.setAttribute("role", "separator");
  splitter.setAttribute("aria-orientation", "horizontal");
  splitter.setAttribute("aria-label", "Resize flame graph");
  bindFlameSplitter(splitter, band);

  const tree = el("div", "flame-tree");
  column.append(zoomBar, band, splitter, tree);
  pane.append(column);
  drawFlame();
  renderTree(tree);
}

function bindFlameSplitter(splitter, band) {
  splitter.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    const startY = event.clientY;
    const startH = band.getBoundingClientRect().height;
    const move = (ev) => {
      const paneH = ($("pane") || document.body).getBoundingClientRect().height || 500;
      const next = Math.max(96, Math.min(paneH - 110, startH + (ev.clientY - startY)));
      state.flameHeight = next;
      band.style.height = next + "px";
      band.style.maxHeight = next + "px";
      drawFlame();
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
  });
}

function flameLayout(rowH, pixelWidth) {
  const rects = [];
  const minimum = 0.5 / Math.max(1, pixelWidth);
  const show = state.query ? matchingIds() : null;
  let maxLevel = 0;

  if (state.zoomNode != null && state.byId.has(state.zoomNode)) {
    const chain = zoomAncestors(state.zoomNode);
    const zoomRoot = chain[chain.length - 1];
    for (let i = 0; i < chain.length - 1 && i < FLAME_LEVELS; i += 1) {
      rects.push({
        node: chain[i],
        x: 0,
        y: i * rowH,
        width: 1,
        height: Math.max(8, rowH - 1),
        isContext: true,
      });
      if (i > maxLevel) maxLevel = i;
    }
    const startLevel = Math.min(FLAME_LEVELS - 1, chain.length - 1);
    const pending = [{
      node: zoomRoot,
      x: 0,
      y: startLevel * rowH,
      width: 1,
      level: startLevel,
    }];
    while (pending.length) {
      const item = pending.pop();
      if (item.level >= FLAME_LEVELS) continue;
      if (item.level > maxLevel) maxLevel = item.level;
      rects.push({
        node: item.node,
        x: item.x,
        y: item.y,
        width: item.width,
        height: Math.max(8, rowH - 1),
        isContext: false,
      });
      const kids = kidsOf(item.node.id).filter((kid) => !show || show.has(kid.id));
      let childCursor = item.x;
      const next = [];
      for (const kid of kids) {
        const childWidth = item.node.weight ? (item.width * kid.weight) / item.node.weight : 0;
        if (childWidth >= minimum) {
          next.push({
            node: kid,
            x: childCursor,
            y: item.y + rowH,
            width: childWidth,
            level: item.level + 1,
          });
        }
        childCursor += childWidth;
      }
      for (let index = next.length - 1; index >= 0; index -= 1) pending.push(next[index]);
    }
    return { rects, levels: maxLevel + 1 };
  }

  const roots = kidsOf("root").filter((node) => !show || show.has(node.id));
  const total = roots.reduce((sum, node) => sum + node.weight, 0) || 1;
  const pending = [];
  let cursor = 0;
  for (const root of roots) {
    const width = root.weight / total;
    pending.push({ node: root, x: cursor, y: 0, width, level: 0 });
    cursor += width;
  }
  while (pending.length) {
    const item = pending.pop();
    if (item.level >= FLAME_LEVELS) continue;
    if (item.level > maxLevel) maxLevel = item.level;
    rects.push({
      node: item.node,
      x: item.x,
      y: item.y,
      width: item.width,
      height: Math.max(8, rowH - 1),
      isContext: false,
    });
    const kids = kidsOf(item.node.id).filter((kid) => !show || show.has(kid.id));
    let childCursor = item.x;
    const next = [];
    for (const kid of kids) {
      const childWidth = item.node.weight ? (item.width * kid.weight) / item.node.weight : 0;
      if (childWidth >= minimum) {
        next.push({
          node: kid,
          x: childCursor,
          y: item.y + rowH,
          width: childWidth,
          level: item.level + 1,
        });
      }
      childCursor += childWidth;
    }
    for (let index = next.length - 1; index >= 0; index -= 1) pending.push(next[index]);
  }
  return { rects, levels: maxLevel + 1 };
}

function flameColor(frame, dark) {
  const table = dark ? FLAME_DARK : FLAME_LIGHT;
  const isMeta = frame.name === "(idle)" || frame.name === "(program)";
  const bucket = isMeta
    ? table.meta
    : (table[frame.runtime] || table.native);
  return bucket[hash(frame.name) % bucket.length];
}

function drawFlame() {
  const canvas = $("flame");
  if (!canvas) return;
  const host = canvas.parentElement;
  const width = Math.max(1, host.clientWidth);
  const rowH = FLAME_ROW;
  const { rects, levels } = flameLayout(rowH, width);
  const height = Math.max(rowH, levels * rowH);
  const ratio = window.devicePixelRatio || 1;
  const dark = isDark();
  canvas.width = Math.floor(width * ratio);
  canvas.height = Math.floor(height * ratio);
  canvas.style.width = width + "px";
  canvas.style.height = height + "px";
  const ctx = canvas.getContext("2d");
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  ctx.clearRect(0, 0, width, height);
  const selectBg = cssColor("--select", dark ? "#0a84ff" : "#0a5fd4");
  const textColor = dark ? "#f5f5f7" : "#18181b";

  for (const rect of rects) {
    const pixel = rect.width * width;
    if (pixel < 0.5) continue;
    const frame = frameOf(rect.node.frame);
    const color = flameColor(frame, dark);
    const selected = rect.node.id === state.selectedNode || (state.selectedNode == null && rect.node.frame === state.selectedFrame);
    const matched = state.query ? frameMatches(rect.node.frame) : true;
    const barH = Math.max(8, rect.height - 1);

    ctx.save();
    if (rect.isContext) ctx.globalAlpha = 0.58;
    else if (state.query && !matched) ctx.globalAlpha = 0.42;

    ctx.fillStyle = selected
      ? selectBg
      : "rgb(" + color[0] + "," + color[1] + "," + color[2] + ")";
    ctx.fillRect(rect.x * width + 0.5, rect.y + 0.5, Math.max(0, pixel - 1), barH);

    if (pixel > 32) {
      ctx.fillStyle = selected ? "#ffffff" : textColor;
      ctx.font = "11px -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif";
      ctx.beginPath();
      ctx.rect(rect.x * width + 2, rect.y, Math.max(0, pixel - 4), barH);
      ctx.clip();
      const loc = formatLocationShort(frame);
      const caption = (pixel > 150 && loc) ? (frame.name + " (" + loc + ")") : frame.name;
      ctx.fillText(caption, rect.x * width + 4, rect.y + Math.min(barH - 4, 13));
    }
    ctx.restore();
  }

  const tip = host.querySelector(".tip");
  canvas.onmousemove = (event) => {
    const hit = hitFlame(rects, event, width);
    if (!hit) {
      tip.hidden = true;
      return;
    }
    const frame = frameOf(hit.node.frame);
    const loc = formatLocationShort(frame);
    const parts = [frame.name];
    if (loc) parts.push(loc);
    parts.push(formatWeight(hit.node.weight) + " (" + formatPercent(hit.node.weight, state.tree.total) + ")");
    if (hit.node.self > 0) parts.push("self " + formatWeight(hit.node.self));
    tip.hidden = false;
    tip.textContent = parts.join("  ·  ");
    tip.style.left = Math.min(event.clientX + 12, window.innerWidth - 460) + "px";
    tip.style.top = event.clientY + 14 + "px";
  };
  canvas.onmouseleave = () => {
    tip.hidden = true;
  };
  canvas.onclick = (event) => {
    const hit = hitFlame(rects, event, width);
    if (!hit) return;
    if (state.zoomNode === hit.node.id) {
      state.zoomNode = null;
    } else {
      state.zoomNode = hit.node.id;
    }
    focusNodeInTree(hit.node);
    updateFlameView();
  };
  canvas.ondblclick = (event) => {
    const hit = hitFlame(rects, event, width);
    if (!hit) return;
    const frame = frameOf(hit.node.frame);
    if (frame.file) openSourceForFrame(hit.node.frame, hit.node.id);
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

function pathThroughSelection() {
  if (state.selectedNode == null || !state.tree) return null;
  const selected = state.byId.get(state.selectedNode);
  if (!selected) return null;
  const seen = new Set();
  const up = [];
  let cursor = selected;
  while (cursor && !seen.has(cursor.id)) {
    seen.add(cursor.id);
    up.push(cursor);
    if (cursor.parent == null) break;
    cursor = state.byId.get(cursor.parent);
  }
  up.reverse();
  const down = [];
  cursor = selected;
  while (cursor) {
    const kids = (state.byParent.get(cursor.id) || []).slice();
    if (!kids.length) break;
    kids.sort((left, right) => Number(right.weight) - Number(left.weight) || left.id - right.id);
    const next = kids[0];
    if (!next || seen.has(next.id)) break;
    seen.add(next.id);
    down.push(next);
    cursor = next;
  }
  return up.concat(down);
}

function renderInspector() {
  const heavy = $("heaviest");
  clear(heavy);
  const through = pathThroughSelection();
  const label = $("heaviest-label");
  const scope = $("heaviest-scope");
  const items = through
    ? through.map((node) => ({ frameId: node.frame, nodeId: node.id, weight: node.weight }))
    : state.tree
      ? state.tree.heaviest.map((frameId) => {
          const row = (state.tree.functions || []).find((f) => f.frame === frameId);
          return { frameId, nodeId: null, weight: row ? row.weight : 0 };
        })
      : [];
  if (label) label.textContent = through ? "Heaviest path" : "Heaviest stack";
  if (scope) {
    scope.textContent = through
      ? "Through the selected function"
      : "Heaviest stack in this view";
  }
  if (!items.length) {
    heavy.append(el("li", "muted", "No stack"));
  } else {
    for (const entry of items) {
      const frame = frameOf(entry.frameId);
      const item = el("li", null, frame.name);
      item.dataset.frame = String(entry.frameId);
      if (entry.nodeId != null) item.dataset.node = String(entry.nodeId);
      if (state.tree && state.tree.total > 0 && entry.weight > 0) {
        item.dataset.share = formatPercent(entry.weight, state.tree.total);
      }
      const fullLoc = formatLocationFull(frame);
      item.title = fullLoc ? frame.name + " — " + fullLoc : frame.name;
      const isSelected = entry.nodeId != null
        ? entry.nodeId === state.selectedNode
        : entry.frameId === state.selectedFrame;
      item.setAttribute("aria-selected", isSelected ? "true" : "false");
      item.addEventListener("click", () => {
        state.selectedFrame = entry.frameId;
        if (entry.nodeId != null) {
          state.selectedNode = entry.nodeId;
        } else {
          const node = state.tree.nodes.find((candidate) => candidate.frame === entry.frameId);
          state.selectedNode = node ? node.id : null;
        }
        renderPane();
        renderInspector();
      });
      heavy.append(item);
    }
  }

  const detail = $("detail");
  const srcBtn = $("open-source-btn");
  clear(detail);
  if (state.selectedFrame == null) {
    if (srcBtn) srcBtn.hidden = true;
    detail.append(el("p", "muted", "Select a function."));
    return;
  }
  const frame = frameOf(state.selectedFrame);
  if (srcBtn) {
    srcBtn.hidden = !frame.file;
    srcBtn.onclick = () => openSourceForFrame(state.selectedFrame, state.selectedNode);
  }
  const row = (state.tree.functions || []).find((item) => item.frame === state.selectedFrame);
  const node = state.byId.get(state.selectedNode);
  addDetail(detail, "Name", frame.name);
  addDetail(detail, "Runtime", frame.runtime || "native");
  if (frame.file) {
    const short = formatLocationShort(frame);
    const full = formatLocationFull(frame);
    detail.append(el("dt", null, "Source"));
    const dd = el("dd");
    const link = el("span", "source-dd-short", short);
    link.title = "Click to open Source view";
    link.addEventListener("click", () => openSourceForFrame(state.selectedFrame, state.selectedNode));
    dd.append(link);
    if (full && full !== short) {
      dd.append(el("span", "source-dd-full", full));
    }
    detail.append(dd);
  }
  if (frame.module) addDetail(detail, "Module", frame.module);
  if (frame.address) addDetail(detail, "Address", frame.address);
  addDetail(detail, "System library", frame.system ? "yes" : "no");
  const weight = node ? node.weight : row ? row.weight : 0;
  const self = node ? node.self : row ? row.self : 0;
  addDetail(detail, "Weight", formatWeight(weight));
  addDetail(detail, "Self", formatWeight(self));
  addDetail(detail, "Share", formatPercent(weight, state.tree.total));
  addDetail(detail, "Self share", formatPercent(self, state.tree.total));
}

function addDetail(detail, name, value) {
  detail.append(el("dt", null, name));
  const cell = el("dd", null, value);
  cell.title = value;
  detail.append(cell);
}

function setPane(name) {
  if (state.pane !== name && state.pane !== "source") {
    state.previousPane = state.pane;
  }
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
    } else if (state.pane === "flame" && state.zoomNode != null) {
      state.zoomNode = null;
      updateFlameView();
    } else if (state.pane === "source") {
      setPane(state.previousPane || "tree");
    } else if (
      state.selection ||
      (state.view &&
        state.session &&
        state.session.timeline &&
        (state.view.start !== state.session.timeline.start || state.view.end !== state.session.timeline.end))
    ) {
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
  if (event.key === "Enter" && state.selectedFrame != null) {
    const frame = frameOf(state.selectedFrame);
    if (frame.file) {
      event.preventDefault();
      openSourceForFrame(state.selectedFrame, state.selectedNode);
      return;
    }
  }
  if (state.pane !== "tree" && state.pane !== "flame") return;
  const flat = state.flat;
  const index = flat.findIndex((node) => node.id === state.selectedNode);
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    const next = event.key === "ArrowDown"
      ? Math.min(flat.length - 1, index + 1)
      : Math.max(0, index < 0 ? 0 : index - 1);
    if (flat[next]) {
      selectNode(flat[next]);
      const row = document.querySelector('.row[data-node="' + flat[next].id + '"]');
      if (row) row.scrollIntoView({ block: "nearest" });
      else if (state.pane === "flame") {
        const host = document.querySelector(".flame-tree");
        if (host) renderTree(host);
      } else {
        renderTree();
      }
    }
  } else if (event.key === "ArrowRight" && index >= 0) {
    event.preventDefault();
    const node = flat[index];
    if ((state.byParent.get(node.id) || []).length && !state.expanded.has(node.id)) {
      state.expanded.add(node.id);
      if (state.pane === "flame") {
        const host = document.querySelector(".flame-tree");
        if (host) renderTree(host);
      } else {
        renderTree();
      }
    }
  } else if (event.key === "ArrowLeft" && index >= 0) {
    event.preventDefault();
    const node = flat[index];
    if (state.expanded.has(node.id)) {
      state.expanded.delete(node.id);
      if (state.pane === "flame") {
        const host = document.querySelector(".flame-tree");
        if (host) renderTree(host);
      } else {
        renderTree();
      }
    } else if (node.parent != null) {
      const parent = state.byId.get(node.parent);
      if (parent) selectNode(parent);
    }
  }
}

function bindSplitter(splitter, axis) {
  if (!splitter) return;
  splitter.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    event.preventDefault();
    const overview = $("overview");
    const inspector = document.querySelector(".inspector");
    const start = axis === "y" ? event.clientY : event.clientX;
    const startSize = axis === "y"
      ? overview.getBoundingClientRect().height
      : inspector.getBoundingClientRect().width;
    const move = (ev) => {
      const delta = (axis === "y" ? ev.clientY : ev.clientX) - start;
      if (axis === "y") {
        const next = Math.max(32, Math.min(window.innerHeight * 0.55, startSize + delta));
        overview.style.flex = "0 0 " + next + "px";
        overview.style.height = next + "px";
        overview.style.maxHeight = next + "px";
      } else {
        const next = Math.max(210, Math.min(window.innerWidth * 0.5, startSize - delta));
        inspector.style.width = next + "px";
      }
      if (state.pane === "flame") drawFlame();
      if (state.session && state.session.timeline) drawTimeline();
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
  });
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
    state.zoomNode = null;
    refreshTree();
  });
  $("hide-system").addEventListener("click", () => {
    if ($("hide-system").disabled) return;
    state.hideSystem = !state.hideSystem;
    $("hide-system").setAttribute("aria-pressed", state.hideSystem ? "true" : "false");
    state.expanded.clear();
    state.zoomNode = null;
    refreshTree();
  });
  window.addEventListener("resize", () => {
    if (state.pane === "flame") drawFlame();
    if (state.session && state.session.timeline) drawTimeline();
  });
  bindSplitter($("split-overview"), "y");
  bindSplitter($("split-inspector"), "x");
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
