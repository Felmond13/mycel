// Tiny dependency-free SVG sparkline. Values are normalized against the
// series max (or an explicit :max), drawn as a line + soft area fill.

export default {
  name: "Sparkline",
  props: {
    values: { type: Array, required: true },
    width: { type: Number, default: 120 },
    height: { type: Number, default: 28 },
    color: { type: String, default: "var(--teal)" },
    max: { type: Number, default: 0 }, // 0 = auto (series max)
  },
  computed: {
    peak() {
      const auto = Math.max(...this.values, 0.0001);
      return this.max > 0 ? Math.max(this.max, auto) : auto;
    },
    points() {
      const v = this.values;
      if (v.length < 2) return "";
      const stepX = this.width / (v.length - 1);
      const pad = 2;
      const h = this.height - pad * 2;
      return v
        .map((y, i) => (i * stepX).toFixed(1) + "," + (pad + h * (1 - y / this.peak)).toFixed(1))
        .join(" ");
    },
    areaPoints() {
      if (!this.points) return "";
      return "0," + this.height + " " + this.points + " " + this.width + "," + this.height;
    },
  },
  template: `
  <svg class="spark" :width="width" :height="height" :viewBox="'0 0 ' + width + ' ' + height"
       preserveAspectRatio="none" aria-hidden="true">
    <polygon v-if="areaPoints" :points="areaPoints" :fill="color" opacity="0.13"></polygon>
    <polyline v-if="points" :points="points" fill="none" :stroke="color"
              stroke-width="1.6" stroke-linejoin="round" stroke-linecap="round"></polyline>
  </svg>`,
};
