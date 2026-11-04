// Premiers pas (vague 2) : emplacement réservé pour le parcours de
// découverte — la navigation est déjà en place.

export default {
  name: "GettingStartedPage",
  template: `
  <section>
    <h1 class="title">Premiers pas</h1>
    <div class="sub">Le tour guidé de Mycel : installer une app, la démarrer isolée, explorer le magasin.</div>
    <empty-state title="Bientôt">
      Ce parcours guidé arrive bientôt. En attendant, la page
      <a href="#apps">Apps</a> permet déjà d'installer et de démarrer une app en deux clics.
    </empty-state>
  </section>`,
};
