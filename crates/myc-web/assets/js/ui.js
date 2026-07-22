// UI chrome state: toasts, the styled confirm dialog, and the logs
// slide-over. Rendered by ToastHost / ConfirmDialog / LogsPanel; opened
// from anywhere via these functions.

import { reactive, nextTick } from "./vue.js";
import { api } from "./api.js";

export const UI = reactive({
  toasts: [],
  confirm: { open: false, title: "", message: "", label: "Confirm", resolve: null },
  logs: { open: false, cid: null, title: "", reference: "", text: "", status: "", running: false, exit: null, follow: true },
});

/* ---- toasts ---- */

const TOAST_MS = 4600;
let toastSeq = 0;

/** toast("saved")  ·  toast("boom", true)  — at most 3 stacked. */
export function toast(msg, isErr) {
  const id = ++toastSeq;
  if (UI.toasts.length >= 3) UI.toasts.shift();
  UI.toasts.push({ id, msg: String(msg), err: !!isErr, ms: TOAST_MS });
  setTimeout(() => {
    const i = UI.toasts.findIndex(t => t.id === id);
    if (i !== -1) UI.toasts.splice(i, 1);
  }, TOAST_MS);
}

/* ---- confirm dialog ---- */

export function confirmDialog(title, message, label) {
  return new Promise(resolve => {
    UI.confirm = { open: true, title, message, label: label || "Confirm", resolve };
  });
}

export function answerConfirm(ok) {
  if (UI.confirm.resolve) UI.confirm.resolve(ok);
  UI.confirm.open = false;
  UI.confirm.resolve = null;
}

/* ---- logs slide-over ---- */

let logTimer = null;
let logOffset = 0;

export function openLogs(cid, title, reference) {
  UI.logs = {
    open: true, cid, title: title || cid, reference: reference || "",
    text: "", status: "…", running: false, exit: null, follow: true,
  };
  logOffset = 0;
  const pull = async () => {
    try {
      const r = await api.get("/api/containers/" + encodeURIComponent(cid) + "/logs?offset=" + logOffset);
      if (r.data) {
        UI.logs.text += r.data;
        if (UI.logs.follow) nextTick(() => {
          const box = document.querySelector(".drawer-body");
          if (box) box.scrollTop = box.scrollHeight;
        });
      }
      logOffset = r.next;
      UI.logs.status = r.status;
      UI.logs.running = r.running;
      UI.logs.exit = r.exit_code;
      if (!r.running && r.next >= r.size) { clearInterval(logTimer); logTimer = null; }
    } catch (_) {
      UI.logs.status = "log unavailable";
      clearInterval(logTimer);
      logTimer = null;
    }
  };
  clearInterval(logTimer);
  pull();
  logTimer = setInterval(pull, 900);
}

export function closeLogs() {
  UI.logs.open = false;
  clearInterval(logTimer);
  logTimer = null;
}
