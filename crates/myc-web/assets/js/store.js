// Shared reactive state + the polling loops that keep it fresh.
// Pages read from `S`; all cross-page actions live here too, so components
// stay thin.

import { reactive } from "./vue.js";
import { api } from "./api.js";
import { canonicalImage } from "./format.js";
import { recordSamples } from "./metrics.js";
import { toast, confirmDialog } from "./ui.js";

export const S = reactive({
  now: Date.now() / 1000,   // ticking clock for uptime displays
  loaded: false,            // first envs+containers fetch completed
  apiDown: false,
  envs: [],
  containers: [],
  stats: null,              // /api/stats (footer + store page)
  appUi: {},                // catalog card transient state {installing, progress, starting}
});

/* ---- polling ---- */

export async function refreshState() {
  try {
    const [envs, cts] = await Promise.all([api.get("/api/envs"), api.get("/api/containers")]);
    S.envs = envs.environments;
    S.containers = cts.containers;
    recordSamples(cts.containers); // rolling CPU/RAM history for sparklines
    S.apiDown = false;
  } catch (_) {
    S.apiDown = true;
  }
  S.loaded = true;
}

export async function refreshStats() {
  try { S.stats = await api.get("/api/stats"); } catch (_) { /* footer shows fallback */ }
}

export function startPolling() {
  refreshState();
  refreshStats();
  setInterval(() => { if (!document.hidden) refreshState(); }, 2500);
  setInterval(() => { if (!document.hidden) refreshStats(); }, 10000);
  setInterval(() => { S.now = Date.now() / 1000; }, 1000);
}

/* ---- derived helpers ---- */

export function isInstalled(image) {
  const canon = canonicalImage(image);
  return S.envs.some(e => e.name === canon || (e.refs || []).includes(image) || (e.refs || []).includes(canon));
}

export function runningContainerFor(image) {
  return S.containers.find(c => c.reference === image && c.running && !c.stack);
}

/* ---- shared actions ---- */

/** Start an ingest job and resolve when it finishes; `onProgress` gets every
    polled job object. */
export function installImageAndWait(image, onProgress) {
  return api.post("/api/ingest", { image }).then(r => new Promise((resolve, reject) => {
    const t = setInterval(async () => {
      try {
        const j = await api.get("/api/jobs/" + r.job);
        if (onProgress) onProgress(j);
        if (j.status === "done") { clearInterval(t); resolve(j); }
        else if (j.status === "failed") { clearInterval(t); reject(new Error(j.message)); }
      } catch (e) { clearInterval(t); reject(e); }
    }, 700);
  }));
}

/** Confirm, then stop a container (used from several pages). */
export async function stopContainer(cid, title) {
  const ok = await confirmDialog("Stop " + (title || "this app") + "?",
    "The app is sent a polite shutdown signal. You can start it again any time.", "Stop it");
  if (!ok) return;
  try {
    await api.post("/api/containers/" + encodeURIComponent(cid) + "/stop");
    toast((title || "App") + " stopped");
    refreshState();
  } catch (e) { toast(e.message, true); }
}
