// Snapshots: which restore lanes this machine offers (Firecracker,
// Cloud Hypervisor under WSL2, warm pool) and the measured restore
// times. Everything on screen comes from GET /api/snapshots/status —
// nothing is simulated; if the endpoint is missing the page degrades
// to a discreet banner.

import { api } from "../api.js";

export default {
  name: "SnapshotsPage",
  data() { return { d: null, error: "", refreshing: false }; },
  computed: {
    fc() {
      const v = this.d && this.d.firecracker;
      if (v && typeof v === "object") return { available: !!v.available, version: v.version || "" };
      return { available: !!v, version: "" };
    },
    ch() {
      // true / false or an object — normalize defensively.
      const v = this.d && this.d.cloud_hypervisor_wsl2;
      if (v === true) return { available: true, note: "" };
      if (v && typeof v === "object") return { available: !!v.available, note: v.note || "" };
      return { available: false, note: "" };
    },
    warm() {
      const v = this.d && this.d.warm_pool;
      if (v && typeof v === "object") return { available: !!v.available, note: v.note || "" };
      return { available: !!v, note: "" };
    },
    restore() {
      const list = (this.d && this.d.measurements) || [];
      return list.find(m => /restore|restaur/i.test(m.label || "")) || null;
    },
    measurements() { return ((this.d && this.d.measurements) || []).slice(); },
  },
  mounted() { this.load(); },
  methods: {
    async load() {
      this.refreshing = true;
      try { this.d = await api.get("/api/snapshots/status"); this.error = ""; }
      catch (e) { this.error = e.message; }
      this.refreshing = false;
    },
    fmtMs(v) {
      const n = Number(v);
      if (!Number.isFinite(n)) return "—";
      if (n >= 1000) return (Math.round(n / 10) / 100).toString().replace(".", ",") + " s";
      return Math.round(n) + " ms";
    },
    fmtWhen(w) {
      if (w === null || w === undefined || w === "") return "—";
      let date = null;
      if (typeof w === "number") {
        // epoch seconds or milliseconds — guess from the magnitude.
        if (w > 1e12) date = new Date(w);
        else if (w > 1e9) date = new Date(w * 1000);
      } else {
        const d = new Date(w);
        if (!isNaN(d.getTime())) date = d;
      }
      if (!date) return String(w);
      return date.toLocaleString("fr-FR", { dateStyle: "medium", timeStyle: "short" });
    },
    fmtN(n) { return (n === null || n === undefined || n === "") ? "—" : n; },
  },
  template: `
  <section>
    <div class="row spread" style="margin-bottom:6px">
      <h1 class="title" style="margin:0">Snapshots</h1>
      <button v-if="d" class="btn sm ghost" :disabled="refreshing" @click="load">
        <span v-if="refreshing" class="spinner"></span>Actualiser
      </button>
    </div>
    <div class="sub">Une microVM peut être figée en mémoire puis restaurée en quelques dizaines de millisecondes — voici les voies disponibles sur cette machine et les temps réellement mesurés.</div>

    <div v-if="error" class="dim-banner">
      Information indisponible<span v-if="error"> — {{ error }}</span>. Cette page a besoin d'une version récente du serveur Mycel.
    </div>
    <skeleton-block v-else-if="!d" h="90px" :n="3"></skeleton-block>
    <template v-else>
      <div v-if="restore" class="card headline-stat">
        <div class="big">{{ fmtMs(restore.median_ms) }}</div>
        <div class="cap">pour restaurer une microVM — médiane mesurée sur {{ fmtN(restore.n) }} essai{{ Number(restore.n) === 1 ? '' : 's' }} (« {{ restore.label }} »)</div>
      </div>

      <div class="section-title">Voies de restauration</div>
      <div class="lane-grid">
        <div class="lane">
          <div class="lane-head">
            <b>Firecracker</b>
            <span class="pill" :class="fc.available ? 'green' : 'gray'">{{ fc.available ? "disponible" : "non disponible" }}</span>
          </div>
          <div class="lane-body">
            <template v-if="fc.available">MicroVM légère à noyau dédié<span v-if="fc.version"> — version <span class="mono">{{ fc.version }}</span></span>.</template>
            <template v-else>Pas accessible sur cette machine pour l'instant.</template>
          </div>
        </div>
        <div class="lane dim">
          <div class="lane-head">
            <b>Cloud Hypervisor <span class="lane-sub">sous WSL2</span></b>
            <span class="pill amber" title="l'état est restauré à l'identique, sans reprise instantanée de l'exécution">restore inerte — mesuré</span>
          </div>
          <div class="lane-body">
            La VM est restaurée à l'identique, sans reprise instantanée de l'exécution — une voie de secours, plus lente.
            <div v-if="ch.note" style="margin-top:4px;color:var(--faint)">{{ ch.note }}</div>
          </div>
        </div>
        <div class="lane" :class="{ dim: !warm.available }">
          <div class="lane-head">
            <b>Pool chaud</b>
            <span class="pill" :class="warm.available ? 'green' : 'gray'">{{ warm.available ? "prêt" : "non disponible" }}</span>
          </div>
          <div class="lane-body">
            {{ warm.note || "Des microVM pré-démarrées attendent d'accueillir vos apps pour un lancement immédiat." }}
          </div>
        </div>
      </div>

      <div class="section-title">Mesures</div>
      <div class="card">
        <table v-if="measurements.length">
          <thead>
            <tr>
              <th>Mesure</th>
              <th class="num">Médiane</th>
              <th class="num">p95</th>
              <th class="num">n</th>
              <th>Date</th>
              <th>Source</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="(m, i) in measurements" :key="i">
              <td>{{ m.label }}</td>
              <td class="num" :class="{ 'tc-teal': m === restore }"><b>{{ fmtMs(m.median_ms) }}</b></td>
              <td class="num">{{ fmtMs(m.p95_ms) }}</td>
              <td class="num">{{ fmtN(m.n) }}</td>
              <td>{{ fmtWhen(m.when) }}</td>
              <td><span class="mono" style="color:var(--faint);font-size:11.5px">{{ m.source || "—" }}</span></td>
            </tr>
          </tbody>
        </table>
        <div v-else style="color:var(--muted);font-size:13px">
          Aucune mesure enregistrée pour l'instant. Elles apparaîtront ici dès le premier benchmark de cette machine.
        </div>
      </div>

      <div class="hint-line">
        Un instantané fige une microVM en mémoire ; la <b>restaurer</b> évite de redémarrer le système invité.
        Tous les chiffres de cette page sont <b>mesurés sur cette machine</b> — rien n'est simulé.
      </div>
    </template>
  </section>`,
};
