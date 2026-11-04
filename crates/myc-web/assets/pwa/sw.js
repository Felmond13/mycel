// Service worker minimal de Mycel — aucune dépendance, aucune magie :
//   - /js/*, /style.css, /vendor/*, /icons/* : cache-first (remplissage
//     paresseux au premier passage) ;
//   - l'index et les navigations : network-first, cache en secours ;
//   - /api/* : toujours le réseau (données fraîches, jamais cachées) ;
//   - hors ligne sans cache : un message honnête plutôt qu'une erreur
//     de navigateur cryptique.

const CACHE = "mycel-v1";
const PRECACHE = [
  "/style.css",
  "/vendor/vue.js",
  "/manifest.webmanifest",
  "/icons/icon-192.png",
  "/icons/icon-512.png",
];

/** Requêtes d'assets statiques : cache-first. */
function isCacheable(pathname) {
  return pathname.startsWith("/js/")
    || pathname === "/style.css"
    || pathname.startsWith("/vendor/")
    || pathname.startsWith("/icons/");
}

/** Réponse de secours hors ligne (texte simple, pas de HTML inventé). */
function offlineFallback() {
  return new Response(
    "Mycel est hors ligne — relancez « myc ui » dans un terminal, puis rechargez cette page.",
    { status: 503, headers: { "Content-Type": "text/plain; charset=utf-8" } },
  );
}

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches.open(CACHE)
      .then((cache) => cache.addAll(PRECACHE))
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (url.origin !== location.origin) return;
  if (url.pathname.startsWith("/api/")) return; // données : réseau, toujours

  if (isCacheable(url.pathname)) {
    // Cache-first avec remplissage paresseux : le premier passage va au
    // réseau, les suivants sont instantanés même hors ligne.
    event.respondWith(
      caches.match(event.request).then(
        (hit) =>
          hit ||
          fetch(event.request).then((res) => {
            const copy = res.clone();
            caches.open(CACHE).then((cache) => cache.put(event.request, copy));
            return res;
          }),
      ),
    );
    return;
  }

  // Index et navigations : network-first, cache en secours, message sinon.
  event.respondWith(
    fetch(event.request)
      .then((res) => {
        if (event.request.mode === "navigate") {
          const copy = res.clone();
          caches.open(CACHE).then((cache) => cache.put("/", copy));
        }
        return res;
      })
      .catch(() =>
        caches
          .match(event.request)
          .then((hit) => hit || caches.match("/"))
          .then((hit) => hit || offlineFallback()),
      ),
  );
});
