// Isolation engines (run modes): which kinds this machine can start
// containers with, from GET /api/engines. Loaded once at startup and
// shared by the Start dialog picker, the stack editor and the running
// list. Older servers without the endpoint simply leave `failed` set —
// every consumer falls back to the quick mode, with no console error.

import { reactive } from "./vue.js";
import { api } from "./api.js";

export const NATIVE = "native-namespaces";
export const KVM = "kvm-microvm";

export const ENGINES = reactive({
  loaded: false,   // fetch settled (success or failure)
  failed: false,   // endpoint missing/unreachable → degraded UI
  default: NATIVE, // the server's default engine kind
  engines: [],     // [{kind, label, available, complete, degraded: []}]
});

let loading = null;

/** Fetch once; concurrent callers share the same promise. */
export function loadEngines() {
  if (loading) return loading;
  loading = api.get("/api/engines")
    .then(d => {
      ENGINES.default = (d && d.default) || NATIVE;
      ENGINES.engines = (d && Array.isArray(d.engines)) ? d.engines : [];
      ENGINES.failed = false;
    })
    .catch(() => { ENGINES.failed = true; })
    .then(() => { ENGINES.loaded = true; });
  return loading;
}
