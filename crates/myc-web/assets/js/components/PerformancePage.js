// Performance : comparatif honnête myc ↔ Docker, mesuré sur cette machine
// via POST/GET /api/bench/compare. Médiane + p95 par scénario et par côté,
// affichées telles quelles — y compris quand Docker gagne (style neutre,
// aucun habillage favorable). Le dernier résultat est conservé par le
// serveur et réaffiché ici au chargement.

import { api } from "../api.js";
import { toast } from "../ui.js";

const SCENARIO_LABELS = {
  "run": "Lancer un conteneur (echo ok)",
  "boot-microvm": "Démarrage d'une microVM (kvm-microvm)",
};

export default {
  name: "PerformancePage",
  data() {
    return {
      state: "idle",     // idle | running | done | error (état serveur)
      progress: 0,       // exécutions chronométrées jusqu'ici
      results: [],       // derniers résultats non vides (conservés en affichage)
      when: null,        // date du run (RFC 3339 côté serveur)
      error: "",         // message d'erreur du job ou de l'endpoint
      unavailable: false,// endpoint absent (serveur plus ancien)
      starting: false,
      timer: null,
    };
  },
  computed: {
    running() { return this.state === "running"; },
    /** Vrai quand le dernier run indique que Docker n'était pas mesurable. */
    dockerAbsent() {
      const run = this.results.find(sc => sc.scenario === "run");
      if (!run) return false;
      if (run.note && /docker/i.test(run.note)) return true;
      return (run.sides || []).some(s => s.label === "docker" && s.error);
    },
    /** Modèle d'affichage : un libellé propre par scénario + ses côtés. */
    view() {
      return this.results.map(sc => ({
        name: SCENARIO_LABELS[sc.scenario] || sc.scenario,
        note: sc.note || "",
        verdict: this.verdict(sc),
        sides: Array.isArray(sc.sides) && sc.sides.length ? sc.sides : null,
      }));
    },
  },
  mounted() { this.refresh(); },
  unmounted() { this.stopPolling(); },
  methods: {
    /** Un GET : reprise d'un run en cours ou réaffichage du dernier résultat. */
    async refresh() {
      try {
        this.apply(await api.get("/api/bench/compare"));
      } catch (e) {
        this.unavailable = true;
        this.error = e.message;
      }
    },
    apply(s) {
      this.state = s.state || "idle";
      this.progress = s.progress || 0;
      if (this.state === "error") this.error = s.error || "";
      else if (this.state !== "idle") this.error = "";
      // Les résultats sont vidés au démarrage d'un run : ne perdre l'affichage
      // du dernier run complet que si le nouveau fournit déjà des données.
      if (Array.isArray(s.results) && (s.results.length || this.state !== "running")) {
        this.results = s.results;
        this.when = s.when || null;
      }
      if (this.running) this.startPolling(); else this.stopPolling();
    },
    async start() {
      this.starting = true;
      try {
        this.apply(await api.post("/api/bench/compare", {}));
      } catch (e) {
        // 409 : un run tourne déjà — reprendre le suivi au lieu d'échouer.
        toast(e.message, true);
        await this.refresh();
      }
      this.starting = false;
    },
    startPolling() {
      if (this.timer) return;
      this.timer = setInterval(async () => {
        try { this.apply(await api.get("/api/bench/compare")); }
        catch (_) { this.stopPolling(); }
      }, 1200);
    },
    stopPolling() {
      if (this.timer) { clearInterval(this.timer); this.timer = null; }
    },
    /** Phrase neutre quand deux côtés ont une médiane : dit juste qui est
        plus rapide, sans couleur ni arrangement — Docker gagne parfois. */
    verdict(sc) {
      const ok = (sc.sides || []).filter(s => Number(s.median_ms) > 0);
      if (ok.length < 2) return "";
      const fmt = r => String(Math.round(r * 10) / 10).replace(".", ",");
      const [a, b] = ok;
      if (a.median_ms < b.median_ms / 1.05) {
        return a.label + " ×" + fmt(b.median_ms / a.median_ms) + " plus rapide";
      }
      if (b.median_ms < a.median_ms / 1.05) {
        return b.label + " ×" + fmt(a.median_ms / b.median_ms) + " plus rapide";
      }
      return "quasi à égalité";
    },
    fmtMs(v) {
      const n = Number(v);
      if (!Number.isFinite(n) || n <= 0) return "—";
      if (n >= 1000) return (Math.round(n / 10) / 100).toString().replace(".", ",") + " s";
      return Math.round(n) + " ms";
    },
    fmtWhen(w) {
      if (!w) return "";
      const d = new Date(w);
      return isNaN(d.getTime()) ? String(w)
        : d.toLocaleString("fr-FR", { dateStyle: "medium", timeStyle: "short" });
    },
  },
  template: `
  <section>
    <h1 class="title">Performance</h1>
    <div class="sub">Combien de temps prend vraiment un démarrage, mesuré sur cette machine — myc contre Docker, médiane et p95 sur mesures brutes.</div>

    <div v-if="unavailable" class="dim-banner">
      Comparatif indisponible<span v-if="error"> — {{ error }}</span>. Cette page a besoin d'une version récente du serveur Mycel.
    </div>
    <template v-else>
      <div class="card">
        <div class="row spread">
          <div>
            <b>Comparer à Docker sur cette machine</b>
            <div style="color:var(--muted);font-size:13px;margin-top:3px">
              10 exécutions chronométrées par côté (alpine:3.20), médiane et p95 calculées sur les échantillons bruts.
              <span v-if="dockerAbsent">Docker n'a pas pu être mesuré au dernier run : les résultats couvrent myc seul.</span>
            </div>
          </div>
          <button class="btn primary" :disabled="running || starting" @click="start">
            <span v-if="running || starting" class="spinner"></span>
            {{ running ? "Mesure en cours…" : "Comparer à Docker sur cette machine" }}
          </button>
        </div>
        <template v-if="running">
          <div class="prog indet" style="margin-top:14px"><i></i></div>
          <div style="color:var(--muted);font-size:12.5px">
            {{ progress }} exécutions chronométrées — la première peut télécharger l'image, les suivantes mesurent.
          </div>
        </template>
      </div>

      <div v-if="error && state === 'error'" class="api-down" style="margin-bottom:16px">{{ error }}</div>

      <template v-if="view.length">
        <div class="section-title">Résultats<span v-if="when" style="text-transform:none;letter-spacing:0;font-weight:500"> — {{ fmtWhen(when) }}</span></div>
        <div class="card">
          <table>
            <thead>
              <tr>
                <th>Scénario</th>
                <th>Côté</th>
                <th class="num">Médiane</th>
                <th class="num">p95</th>
                <th class="num">n</th>
              </tr>
            </thead>
            <tbody>
              <template v-for="sc in view" :key="sc.name">
                <tr v-if="!sc.sides">
                  <td><b>{{ sc.name }}</b></td>
                  <td colspan="4" style="color:var(--muted)">{{ sc.note || "—" }}</td>
                </tr>
                <template v-else>
                  <tr v-for="(side, i) in sc.sides" :key="sc.name + '-' + i">
                    <td v-if="i === 0" :rowspan="sc.sides.length">
                      <b>{{ sc.name }}</b>
                      <div v-if="sc.verdict" class="bench-verdict">{{ sc.verdict }}</div>
                    </td>
                    <td><span class="mono">{{ side.label }}</span></td>
                    <template v-if="Number(side.median_ms) > 0">
                      <td class="num"><b>{{ fmtMs(side.median_ms) }}</b></td>
                      <td class="num">{{ fmtMs(side.p95_ms) }}</td>
                      <td class="num">{{ side.n }}</td>
                    </template>
                    <td v-else colspan="3" class="tc-red" style="font-size:12.5px">
                      {{ side.error || "aucune mesure valide" }}
                      <span v-if="side.measured_before_failure">({{ side.measured_before_failure }} mesures valides avant l'échec)</span>
                    </td>
                  </tr>
                  <tr v-if="sc.note">
                    <td colspan="5" class="bench-note">{{ sc.note }}</td>
                  </tr>
                </template>
              </template>
            </tbody>
          </table>
        </div>
      </template>
      <empty-state v-else-if="state === 'idle'" title="Aucune mesure pour l'instant">
        Lancez la comparaison : les résultats de cette machine apparaîtront ici, tels quels.
      </empty-state>

      <div class="hint-line">
        Chaque chiffre est <b>mesuré sur cette machine</b>, rien de simulé ni de lissé — quand Docker gagne,
        le tableau le dit. Le p95 suit la méthode des verdicts bench (valeur au rang ceil(0,95·n)).
      </div>
    </template>
  </section>`,
};
