// Onboarding « Premiers pas » : mémorisation en localStorage et
// déclenchement automatique quand le magasin est vide. Les accès au
// stockage sont tolérants aux environnements qui ne l'ont pas (tests
// node, navigation privée) — l'onboarding se comporte alors comme
// jamais complété, ce qui reste sans conséquence.

import { S } from "./store.js";

const DONE_KEY = "mycel.onboarding.done";
const SEEN_KEY = "mycel.onboarding.seen";

export function onboardingDone() {
  try { return localStorage.getItem(DONE_KEY) === "1"; } catch (_) { return false; }
}

export function completeOnboarding() {
  try {
    localStorage.setItem(DONE_KEY, "1");
    localStorage.setItem(SEEN_KEY, "1");
  } catch (_) { /* stockage indisponible : tant pis pour la mémoire */ }
}

function onboardingSeen() {
  try { return localStorage.getItem(SEEN_KEY) === "1"; } catch (_) { return false; }
}

/**
 * À appeler une fois au démarrage : quand la première interrogation
 * réussie de /api/envs révèle un magasin vide (machine neuve), envoie
 * vers le parcours guidé. Jamais deux fois (drapeau « vu »), jamais
 * après complétion, et jamais si l'utilisateur a déjà navigué ailleurs.
 */
export function maybeStartOnboarding() {
  const t = setInterval(() => {
    if (!S.loaded) return;
    clearInterval(t);
    if (S.apiDown || S.envs.length > 0 || S.containers.length > 0) return;
    if (onboardingDone() || onboardingSeen()) return;
    try { localStorage.setItem(SEEN_KEY, "1"); } catch (_) { /* voir plus haut */ }
    if (location.hash === "" || location.hash === "#apps") location.hash = "started";
  }, 400);
  // Filet de sécurité : ne pas garder le minuteur en vie indéfiniment.
  setTimeout(() => clearInterval(t), 20000);
}
