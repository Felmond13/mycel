// Two-card isolation picker: "Rapide" (shared kernel, like Docker) vs
// "Isolé — microVM" (dedicated kernel, shield). Availability and the
// default come from GET /api/engines; when that endpoint is missing
// (older server) the picker degrades to quick-only with a discreet note
// instead of an error. Usage: <engine-picker v-model="kind"></engine-picker>

import { ENGINES, loadEngines, NATIVE, KVM } from "../engines.js";

// French copy for the two kinds we know about; unknown kinds still show
// (using their server label) so nothing becomes unreachable.
const FALLBACK = {
  [NATIVE]: { label: "Rapide", hint: "noyau partagé, comme Docker" },
  [KVM]: { label: "Isolé — microVM", hint: "noyau dédié à cette app" },
};

export default {
  name: "EnginePicker",
  props: { modelValue: { type: String, default: "" } },
  emits: ["update:modelValue"],
  async mounted() {
    await loadEngines();
    // Settle the default (engines.default) and snap back to a usable
    // engine if the current one turns out to be unavailable here.
    const cur = this.cards.find(c => c.kind === this.modelValue);
    if (!cur || !cur.available) this.$emit("update:modelValue", this.defaultKind);
  },
  computed: {
    cards() {
      const byKind = {};
      for (const e of ENGINES.engines) byKind[e.kind] = e;
      return [NATIVE, KVM].map(kind => {
        const srv = byKind[kind];
        const fb = FALLBACK[kind] || { label: kind, hint: "" };
        let available, why = "";
        if (ENGINES.failed) {
          // No /api/engines on this server: assume the classic quick mode.
          available = kind === NATIVE;
          if (kind !== NATIVE) why = "informations du serveur indisponibles";
        } else if (srv) {
          available = !!srv.available;
          why = (Array.isArray(srv.degraded) ? srv.degraded : []).join(" · ");
        } else {
          available = false;
          why = "non proposé par ce serveur";
        }
        return {
          kind,
          label: fb.label,
          hint: fb.hint,
          available,
          why,
          partial: !!(srv && srv.available && srv.complete === false),
          strong: kind === KVM, // « isolement renforcé »
        };
      });
    },
    defaultKind() {
      if (!ENGINES.failed && ENGINES.engines.some(e => e.kind === ENGINES.default && e.available)) {
        return ENGINES.default;
      }
      return NATIVE;
    },
    degradedInfo() { return ENGINES.failed; },
  },
  methods: {
    pick(c) {
      if (!c.available) return;
      this.$emit("update:modelValue", c.kind);
    },
  },
  template: `
  <div>
    <div class="engine-grid">
      <div v-for="c in cards" :key="c.kind" class="engine-card"
           :class="{ sel: modelValue === c.kind, off: !c.available }"
           role="button" :aria-pressed="modelValue === c.kind" @click="pick(c)">
        <div class="eh">
          <svg v-if="c.kind === 'kvm-microvm'" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4">
            <path d="M8 2 12.8 3.8v3.9c0 2.9-2.2 5.1-4.8 6-2.6-.9-4.8-3.1-4.8-6V3.8L8 2z"/>
            <path d="M5.9 8.1l1.5 1.5 2.8-3"/>
          </svg>
          <svg v-else viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4">
            <path d="M9 2 4.5 9h2.9L6.5 14 12 6.5H8.8L9 2z"/>
          </svg>
          <span>{{ c.label }}</span>
          <span v-if="c.strong && c.available" class="pill accent" title="un noyau rien que pour cette app">isolement renforcé</span>
        </div>
        <div class="hint">{{ c.hint }}</div>
        <div v-if="c.why" class="why" :class="{ amber: c.partial }">{{ c.why }}</div>
      </div>
    </div>
    <div v-if="degradedInfo" class="engine-note">
      Informations d'isolation indisponibles — ce serveur Mycel ne connaît pas encore les moteurs d'isolation.
    </div>
  </div>`,
};
