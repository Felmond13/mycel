// Thin ES-module facade over the global Vue build (served from
// /vendor/vue.js, loaded before the module graph). Components import from
// here so no file ever touches `window.Vue` directly — and swapping in a
// bundled Vue later would only change this one file.
const Vue = window.Vue;

export const createApp = Vue.createApp;
export const reactive = Vue.reactive;
export const computed = Vue.computed;
export const ref = Vue.ref;
export const nextTick = Vue.nextTick;

export default Vue;
