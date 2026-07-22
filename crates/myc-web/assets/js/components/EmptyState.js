// Friendly empty state that teaches the next step.
// <empty-state title="Nothing here yet">explanation… (slot)</empty-state>

export default {
  name: "EmptyState",
  props: { title: { type: String, default: "" } },
  template: `
  <div class="empty">
    <div v-if="title" class="big">{{ title }}</div>
    <slot></slot>
  </div>`,
};
