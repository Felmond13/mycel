// Performance (vague 2) : emplacement réservé. La comparaison de
// démarrage entre moteurs (POST/GET /api/bench/compare) sera branchée
// ici par la suite — la navigation est déjà en place.

export default {
  name: "PerformancePage",
  template: `
  <section>
    <h1 class="title">Performance</h1>
    <div class="sub">Comparatifs de démarrage et de restore entre les modes d'isolation, mesurés sur cette machine.</div>
    <empty-state title="Bientôt">
      Cette page arrive bientôt : combien de temps prend vraiment chaque mode de démarrage,
      mesuré sur votre machine — rien de simulé.
    </empty-state>
  </section>`,
};
