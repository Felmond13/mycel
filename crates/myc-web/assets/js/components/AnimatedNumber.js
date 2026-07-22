// Animated counter: eases toward its target whenever the value changes.
// <anim-num :value="1234" fmt="bytes|ratio|int">

import { fmtBytes } from "../format.js";

const REDUCED_MOTION = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

export default {
  name: "AnimatedNumber",
  props: {
    value: { type: Number, default: 0 },
    fmt: { type: String, default: "int" },
  },
  data() { return { shown: 0, raf: null }; },
  computed: {
    text() {
      if (this.fmt === "bytes") return fmtBytes(this.shown);
      if (this.fmt === "ratio") return this.shown.toFixed(2) + "\u00d7";
      return Math.round(this.shown).toLocaleString("en-US");
    },
  },
  watch: { value() { this.animate(); } },
  mounted() { this.animate(); },
  unmounted() { cancelAnimationFrame(this.raf); },
  methods: {
    animate() {
      cancelAnimationFrame(this.raf);
      if (REDUCED_MOTION) { this.shown = this.value; return; }
      const from = this.shown, to = this.value, t0 = performance.now(), dur = 650;
      const step = t => {
        const p = Math.min(1, (t - t0) / dur);
        const ease = 1 - Math.pow(1 - p, 3);
        this.shown = from + (to - from) * ease;
        if (p < 1) this.raf = requestAnimationFrame(step);
      };
      this.raf = requestAnimationFrame(step);
    },
  },
  template: '<span>{{ text }}</span>',
};
