// Root layout: mycelium canvas, sidebar, the routed page (with a soft
// transition), and the global chrome (toasts, confirm dialog, logs panel).

import { route, PAGES } from "../router.js";
import { UI, closeLogs, answerConfirm } from "../ui.js";
import { S } from "../store.js";

export default {
  name: "AppShell",
  computed: {
    view() { return PAGES[route.page]; },
    viewKey() { return route.page + "/" + route.arg; },
    arg() { return route.arg; },
    // Only warn once the first load has succeeded — a banner during
    // startup would just flash.
    apiDown() { return S.loaded && S.apiDown; },
  },
  mounted() {
    this.onKey = ev => {
      if (ev.key === "Escape") {
        if (UI.logs.open) closeLogs();
        else if (UI.confirm.open) answerConfirm(false);
      }
    };
    document.addEventListener("keydown", this.onKey);
  },
  unmounted() { document.removeEventListener("keydown", this.onKey); },
  template: `
  <network-canvas></network-canvas>
  <div class="shell">
    <side-bar></side-bar>
    <main>
      <div v-if="apiDown" class="api-down">
        Cannot reach the Mycel server — is <code>myc ui</code> still running? Retrying…
      </div>
      <Transition name="page" mode="out-in">
        <component :is="view" :arg="arg" :key="viewKey"></component>
      </Transition>
    </main>
  </div>
  <toast-host></toast-host>
  <confirm-dialog></confirm-dialog>
  <logs-panel></logs-panel>`,
};
