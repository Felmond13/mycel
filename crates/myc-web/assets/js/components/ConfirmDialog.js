// Styled replacement for window.confirm(), driven by ui.confirmDialog().

import { UI, answerConfirm } from "../ui.js";

export default {
  name: "ConfirmDialog",
  computed: {
    c() { return UI.confirm; },
  },
  methods: { answerConfirm },
  template: `
  <Transition name="fade">
    <div v-if="c.open" class="overlay" @click.self="answerConfirm(false)">
      <div class="dialog">
        <h3>{{ c.title }}</h3>
        <p>{{ c.message }}</p>
        <div class="row">
          <button class="btn ghost" @click="answerConfirm(false)">Cancel</button>
          <button class="btn danger" @click="answerConfirm(true)">{{ c.label }}</button>
        </div>
      </div>
    </div>
  </Transition>`,
};
