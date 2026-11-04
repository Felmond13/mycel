// Doctor : vérifications de la machine (checks PASS/FAIL + correctifs),
// section « moteurs d'isolation » (tiers depuis /api/engines :
// disponible / complet / dégradé) et état Firecracker
// (/api/snapshots/status). Chaque section se dégrade proprement si son
// endpoint manque (serveur plus ancien) — bannière discrète, pas d'erreur.

import { api } from "../api.js";
import { ENGINES, loadEngines } from "../engines.js";

/** Libellé français lisible pour un kind de moteur. */
const ENGINE_NAMES = {
  "kvm-microvm": "microVM KVM (noyau dédié)",
  "hvf-microvm": "microVM Hypervisor.framework (macOS)",
  "wsl2-twin": "Jumeau Linux WSL2",
  "native-namespaces": "Namespaces natifs (partagé)",
};

export default {
  name: "DoctorPage",
  data() {
    return {
      d: null,          // /api/doctor
      error: "",
      rerunning: false,
      fc: null,         // firecracker (extrait de /api/snapshots/status)
      fcError: "",
    };
  },
  computed: {
    ENGINES() { return ENGINES; },
    engines() { return ENGINES.engines; },
  },
  mounted() { this.run(); this.loadFirecracker(); loadEngines(); },
  methods: {
    async run() {
      this.rerunning = true;
      try { this.d = await api.get("/api/doctor"); this.error = ""; }
      catch (e) { this.error = e.message; }
      this.rerunning = false;
    },
    async loadFirecracker() {
      try {
        const s = await api.get("/api/snapshots/status");
        this.fc = (s && s.firecracker) || {};
        this.fcError = "";
      } catch (e) { this.fcError = e.message; }
    },
    engineName(e) { return ENGINE_NAMES[e.kind] || e.kind; },
    isDefault(e) { return ENGINES.default === e.kind; },
    /** Pill de tier : complet > dégradé > indisponible. */
    engineState(e) {
      if (e.available && e.complete) return { cls: "green", label: "disponible — complet" };
      if (e.available) return { cls: "amber", label: "disponible — dégradé" };
      return { cls: "gray", label: "indisponible" };
    },
    /** Checks de sonde en échec pour un moteur (explique pourquoi off). */
    probeFails(e) {
      return ((e.probe && e.probe.checks) || []).filter(c => !c.ok);
    },
  },
  template: `
  <section>
    <h1 class="title">Doctor</h1>
    <div class="sub">Vérifie que cette machine sait télécharger et exécuter des apps, avec un correctif pour ce qui cloche.</div>

    <empty-state v-if="error" :title="error"></empty-state>
    <skeleton-block v-else-if="!d" h="60px" :n="5"></skeleton-block>
    <div v-else class="card">
      <div v-for="c in d.checks" :key="c.name" class="check">
        <span class="pill" :class="c.ok ? 'green' : 'red'">{{ c.ok ? 'PASS' : 'FAIL' }}</span>
        <div>
          <b>{{ c.name }}</b> — <span style="color:var(--muted)">{{ c.detail }}</span>
          <div v-if="c.fix" class="fix">correctif : <code>{{ c.fix }}</code></div>
        </div>
      </div>
      <div class="row spread" style="margin-top:14px">
        <span style="color:var(--muted);font-size:13px">
          {{ d.ok ? "Tout fonctionne — cette machine peut télécharger et exécuter des apps."
                  : "Corrigez les vérifications en échec ci-dessus, puis relancez." }}
        </span>
        <button class="btn sm ghost" :disabled="rerunning" @click="run">
          <span v-if="rerunning" class="spinner"></span>Relancer les vérifications
        </button>
      </div>
    </div>

    <div class="section-title">Moteurs d'isolation</div>
    <div v-if="ENGINES.loaded && !ENGINES.failed" class="lane-grid">
      <div v-for="e in engines" :key="e.kind" class="lane" :class="{ dim: !e.available }">
        <div class="lane-head">
          <b>{{ engineName(e) }}<span v-if="isDefault(e)" class="lane-sub"> · par défaut</span></b>
          <span class="pill" :class="engineState(e).cls">{{ engineState(e).label }}</span>
        </div>
        <div class="lane-body">
          <div class="mono" style="font-size:11px;color:var(--faint);margin-bottom:4px">{{ e.kind }}</div>
          <template v-if="e.degraded && e.degraded.length">
            Limites connues :
            <ul class="degraded-list">
              <li v-for="dgr in e.degraded" :key="dgr">{{ dgr }}</li>
            </ul>
          </template>
          <template v-else>Aucune limite connue sur ce tier.</template>
          <template v-if="!e.available && probeFails(e).length">
            <div v-for="c in probeFails(e)" :key="c.name" style="margin-top:6px">
              <span style="color:var(--red);font-size:12.5px">{{ c.name }}</span>
              <span style="color:var(--muted);font-size:12.5px"> — {{ c.detail }}</span>
              <div v-if="c.fix" class="fix">correctif : <code>{{ c.fix }}</code></div>
            </div>
          </template>
        </div>
      </div>
    </div>
    <div v-else class="dim-banner">
      Informations des moteurs indisponibles<span v-if="ENGINES.loaded && ENGINES.failed"> — ce serveur n'expose pas /api/engines</span>.
    </div>

    <div class="section-title">Firecracker</div>
    <div v-if="fc" class="lane-grid">
      <div class="lane" :class="{ dim: !fc.available }">
        <div class="lane-head">
          <b>Firecracker</b>
          <span class="pill" :class="fc.available ? 'green' : 'gray'">{{ fc.available ? "installé" : "non installé" }}</span>
        </div>
        <div class="lane-body">
          <template v-if="fc.available">
            MicroVM légère à noyau dédié<span v-if="fc.version"> — version <span class="mono">{{ fc.version }}</span></span>.
            <div v-if="fc.path" class="mono" style="font-size:11px;color:var(--faint);margin-top:4px">{{ fc.path }}</div>
          </template>
          <template v-else>
            {{ fc.note || "Pas accessible sur cette machine pour l'instant." }}
            <div style="margin-top:4px">Le moteur <span class="mono">kvm-microvm</span> en a besoin pour démarrer des microVM.</div>
          </template>
        </div>
      </div>
    </div>
    <div v-else-if="fcError" class="dim-banner">
      État Firecracker indisponible — ce serveur n'expose peut-être pas /api/snapshots/status.
    </div>
    <skeleton-block v-else h="90px" :n="1"></skeleton-block>
  </section>`,
};
