// PWA : enregistrement du service worker (/sw.js) et bouton
// d'installation discret, apparu seulement quand le navigateur émet
// beforeinstallprompt. Tout est sans effet quand l'API manque (http
// non-localhost, navigateur non compatible, tests node).

import { reactive } from "./vue.js";

export const PWA = reactive({
  canInstall: false,   // beforeinstallprompt reçu, bouton visible
  installed: false,    // appinstalled reçu
  promptEvent: null,   // l'événement gardé pour déclencher prompt()
});

/** À appeler une fois au démarrage (après le montage de l'app). */
export function initPWA() {
  if (typeof navigator !== "undefined" && navigator.serviceWorker) {
    navigator.serviceWorker.register("/sw.js").catch(() => { /* mode hors ligne indisponible : silencieux */ });
  }
  if (typeof window === "undefined") return;
  window.addEventListener("beforeinstallprompt", ev => {
    ev.preventDefault();          // pas de mini-bandeau intrusive du navigateur
    PWA.promptEvent = ev;
    PWA.canInstall = true;
  });
  window.addEventListener("appinstalled", () => {
    PWA.canInstall = false;
    PWA.promptEvent = null;
    PWA.installed = true;
  });
}

/** Bouton « Installer l'application » : montre la boîte native. */
export async function promptInstall() {
  if (!PWA.promptEvent) return;
  PWA.promptEvent.prompt();
  await PWA.promptEvent.userChoice.catch(() => null);
  PWA.canInstall = false;
  PWA.promptEvent = null;
}
