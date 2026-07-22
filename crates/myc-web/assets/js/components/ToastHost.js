// Bottom-right toast notifications: kind icon, auto-dismiss progress bar,
// at most 3 stacked (enforced by ui.js).

import { UI } from "../ui.js";

export default {
  name: "ToastHost",
  computed: {
    toasts() { return UI.toasts; },
  },
  template: `
  <div class="toasts">
    <TransitionGroup name="toast">
      <div v-for="t in toasts" :key="t.id" class="toast" :class="{ err: t.err }">
        <span class="ic" aria-hidden="true">
          <svg v-if="t.err" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="8" cy="8" r="6.4"/><path d="M8 4.8v3.9M8 11.3v.1"/></svg>
          <svg v-else viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M3.5 8.5 6.5 11.5 12.5 5"/></svg>
        </span>
        <span class="tx">{{ t.msg }}</span>
        <i class="life" :style="{ animationDuration: t.ms + 'ms' }"></i>
      </div>
    </TransitionGroup>
  </div>`,
};
