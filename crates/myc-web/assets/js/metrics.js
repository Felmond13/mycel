// Client-side rolling history of container metrics (last ~30 poll samples,
// i.e. ~75 s at the 2.5 s poll rate). store.js feeds it on every refresh;
// sparklines read from it. Pure state, no DOM.

import { reactive } from "./vue.js";

const KEEP = 30;

/** id -> { cpu: [n…], mem: [n…] } — reactive so sparklines redraw. */
export const M = reactive({ hist: {} });

/** Record one poll's containers; prunes history of ids no longer listed. */
export function recordSamples(containers) {
  const seen = new Set();
  for (const c of containers || []) {
    seen.add(c.id);
    if (!c.running || typeof c.cpu_percent !== "number") continue;
    const h = M.hist[c.id] || (M.hist[c.id] = { cpu: [], mem: [] });
    h.cpu.push(c.cpu_percent);
    h.mem.push(c.memory_bytes || 0);
    if (h.cpu.length > KEEP) h.cpu.shift();
    if (h.mem.length > KEEP) h.mem.shift();
  }
  for (const id of Object.keys(M.hist)) {
    if (!seen.has(id)) delete M.hist[id];
  }
}

/** History for one container ({cpu: [], mem: []}; empty arrays if none). */
export function historyFor(id) {
  return M.hist[id] || { cpu: [], mem: [] };
}

/** "2.4%" — one decimal below 10, integers above. >100% = several cores. */
export function fmtCpu(v) {
  if (typeof v !== "number") return "–";
  return (v >= 10 ? Math.round(v) : v.toFixed(1)) + "%";
}

/** Header totals for the Your Apps strip. */
export function totals(containers) {
  let running = 0, cpu = 0, mem = 0;
  for (const c of containers || []) {
    if (!c.running) continue;
    running++;
    if (typeof c.cpu_percent === "number") cpu += c.cpu_percent;
    if (typeof c.memory_bytes === "number") mem += c.memory_bytes;
  }
  return { running, cpu, mem };
}
