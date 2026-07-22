// One tile of a stat grid, with an animated numeric value.
// <stat-card :value="42" label="files" fmt="bytes" variant="grad|good">
// For non-numeric values pass `text` instead of `value`.

export default {
  name: "StatCard",
  props: {
    value: { type: Number, default: null },
    text: { type: String, default: "" },
    label: { type: String, required: true },
    fmt: { type: String, default: "int" },
    variant: { type: String, default: "" },
  },
  template: `
  <div class="stat">
    <div class="v" :class="variant">
      <anim-num v-if="value !== null" :value="value" :fmt="fmt"></anim-num>
      <template v-else>{{ text }}</template>
    </div>
    <div class="k">{{ label }}</div>
  </div>`,
};
