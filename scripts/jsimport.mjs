// Load the real module graph (main.js and everything it imports) under
// node with stubbed browser globals + a stubbed Vue. Catches broken import
// paths and module-evaluation errors that a pure syntax check cannot.
// Usage: node scripts/jsimport.mjs [assets/js dir]
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = process.argv[2] || join(import.meta.dirname, "..", "crates", "myc-web", "assets", "js");

const appStub = {
  component() { return this; },
  mount() { return this; },
  config: { globalProperties: {} },
};
const vueStub = {
  createApp: () => appStub,
  reactive: x => x,
  computed: fn => fn,
  ref: v => ({ value: v }),
  nextTick: fn => (fn ? fn() : Promise.resolve()),
};

globalThis.window = {
  Vue: vueStub,
  matchMedia: () => ({ matches: true }), // reduced motion: no rAF loops
  addEventListener: () => {},
  removeEventListener: () => {},
  innerWidth: 1280,
  innerHeight: 800,
  devicePixelRatio: 1,
};
globalThis.document = {
  hidden: true, // polling loops stay idle
  addEventListener: () => {},
  removeEventListener: () => {},
  querySelector: () => null,
};
globalThis.location = { hash: "" };

try {
  await import(pathToFileURL(join(root, "main.js")));
  console.log("module graph loaded: main.js and all imports evaluated");
  process.exit(0);
} catch (e) {
  console.error("MODULE GRAPH FAILED: " + e.stack);
  process.exit(1);
}
