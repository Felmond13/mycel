// « Premiers pas » : parcours guidé en 3 écrans — 1) l'état de la
// machine (doctor + moteurs d'isolation), 2) installer une app (lien
// vers le catalogue), 3) la voir tourner (lien vers les conteneurs).
// Déclenché automatiquement quand le magasin est vide (voir
// onboarding.js) ; la complétion est mémorisée et la page reste
// accessible depuis l'aide en bas de la barre latérale.

import { api } from "../api.js";
import { ENGINES, loadEngines, NATIVE } from "../engines.js";
import { completeOnboarding, onboardingDone } from "../onboarding.js";

const STEPS = [
  { title: "Votre machine est prête" },
  { title: "Installer une app" },
  { title: "La voir tourner" },
];

export default {
  name: "GettingStartedPage",
  data() {
    return {
      step: 0,
      d: null,          // /api/doctor (peut rester null si indisponible)
      doctorError: "",
      alreadyDone: false,
    };
  },
  computed: {
    ENGINES() { return ENGINES; },
    steps() { return STEPS; },
    failing() { return ((this.d && this.d.checks) || []).filter(c => !c.ok); },
    enginesReady() {
      return ENGINES.engines.filter(e => e.available && e.complete).length;
    },
    defaultEngineLabel() {
      return ENGINES.default === NATIVE ? "native-namespaces (rapide, partagé)" : ENGINES.default;
    },
  },
  mounted() {
    this.alreadyDone = onboardingDone();
    this.run();
    loadEngines(); // état frais pour l'écran 1 (déjà chargé au démarrage sinon)
  },
  methods: {
    async run() {
      try { this.d = await api.get("/api/doctor"); this.doctorError = ""; }
      catch (e) { this.doctorError = e.message; }
    },
    next() { if (this.step < STEPS.length - 1) this.step++; },
    back() { if (this.step > 0) this.step--; },
    /** Quitter le parcours sans le finir : mémorisé pour ne plus jamais
        revenir tout seul (la page reste atteignable depuis l'aide). */
    skip() {
      completeOnboarding();
      location.hash = "apps";
    },
    /** Étape finale : mémorise la complétion puis montre les conteneurs. */
    finish() {
      completeOnboarding();
      location.hash = "running";
    },
  },
  template: `
  <section>
    <div class="row spread" style="margin-bottom:6px">
      <h1 class="title" style="margin:0">Premiers pas</h1>
      <button class="btn sm ghost" @click="skip">Passer</button>
    </div>
    <div class="sub">Trois écrans, deux minutes : vérifier la machine, installer une app, la voir tourner.</div>

    <div class="gs-progress">
      <div class="gs-dots">
        <span v-for="(s, i) in steps" :key="i" class="gs-dot" :class="{ on: i === step, past: i < step }"></span>
      </div>
      <span class="gs-stepnum">Étape {{ step + 1 }} sur {{ steps.length }}</span>
    </div>

    <!-- Écran 1 : état de la machine -->
    <div v-if="step === 0" class="card">
      <h2 class="gs-title">{{ steps[0].title }}</h2>
      <p class="gs-text">Mycel vérifie que cette machine peut télécharger et exécuter des apps isolées.</p>

      <template v-if="d">
        <div class="row" style="margin-bottom:10px">
          <span class="pill" :class="d.ok ? 'green' : 'amber'">{{ d.ok ? "Machine prête" : failing.length + " vérification" + (failing.length > 1 ? "s" : "") + " à corriger" }}</span>
        </div>
        <div v-for="c in failing" :key="c.name" class="check">
          <span class="pill red">FAIL</span>
          <div>
            <b>{{ c.name }}</b> — <span style="color:var(--muted)">{{ c.detail }}</span>
            <div v-if="c.fix" class="fix">correctif : <code>{{ c.fix }}</code></div>
          </div>
        </div>
      </template>
      <div v-else-if="doctorError" class="dim-banner" style="margin-bottom:0">
        L'état de la machine est momentanément indisponible — vous pouvez continuer,
        la page <a href="#doctor">Doctor</a> y reviendra.
      </div>
      <skeleton-block v-else h="52px" :n="3"></skeleton-block>

      <div class="section-title" style="margin-top:22px">Moteurs d'isolation</div>
      <div v-if="ENGINES.loaded && !ENGINES.failed" style="color:var(--muted);font-size:13px">
        {{ enginesReady }} moteur{{ enginesReady > 1 ? "s" : "" }} complet{{ enginesReady > 1 ? "s" : "" }} sur {{ ENGINES.engines.length }} détecté{{ ENGINES.engines.length > 1 ? "s" : "" }} —
        par défaut : <span class="mono">{{ defaultEngineLabel }}</span>.
      </div>
      <div v-else-if="ENGINES.failed" class="dim-banner" style="margin-bottom:0">
        Informations des moteurs indisponibles sur ce serveur — le mode natif reste utilisable.
      </div>

      <div class="gs-actions">
        <span></span>
        <button class="btn primary" @click="next">Continuer →</button>
      </div>
    </div>

    <!-- Écran 2 : installer une app -->
    <div v-else-if="step === 1" class="card">
      <h2 class="gs-title">{{ steps[1].title }}</h2>
      <p class="gs-text">
        Le catalogue propose des apps prêtes en un clic : Nginx, Redis, PostgreSQL…
        Installer télécharge l'app une fois et range chaque fichier une seule fois sur le disque.
      </p>
      <div class="row">
        <a href="#apps"><button class="btn primary">Ouvrir le catalogue d'apps →</button></a>
      </div>
      <div class="gs-actions">
        <button class="btn ghost" @click="back">← Retour</button>
        <button class="btn primary" @click="next">Continuer →</button>
      </div>
    </div>

    <!-- Écran 3 : la voir tourner -->
    <div v-else class="card">
      <h2 class="gs-title">{{ steps[2].title }}</h2>
      <p class="gs-text">
        Une app installée démarre en un clic, isolée du reste de cette machine.
        La page « Running » les liste : état, consommation, journaux, arrêt.
      </p>
      <div class="row">
        <button class="btn primary" @click="finish">Voir mes apps tourner →</button>
        <a href="#apps"><button class="btn ghost">Retour au catalogue</button></a>
      </div>
      <div class="gs-actions">
        <button class="btn ghost" @click="back">← Retour</button>
        <span v-if="alreadyDone" style="color:var(--faint);font-size:12px">parcours déjà complété — merci d'y revenir</span>
      </div>
    </div>

    <div class="hint-line">
      Ce parcours ne se déclenchera plus tout seul — on y revient à tout moment
      depuis l'aide en bas de la barre latérale.
    </div>
  </section>`,
};
