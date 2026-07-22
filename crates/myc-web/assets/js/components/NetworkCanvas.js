// The living mycelium background: sparse glowing nodes joined by faint
// filaments, drifting slowly. Ambiance only — capped at ~30 fps, paused
// while the tab is hidden, and a single static frame under
// prefers-reduced-motion.

const REDUCED_MOTION = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
const COLORS = [
  [52, 226, 205],   // teal
  [89, 168, 245],   // blue
  [139, 124, 246],  // violet
];
const LINK_DIST = 170;

export default {
  name: "NetworkCanvas",
  mounted() {
    this.nodes = [];
    this.raf = null;
    this.last = 0;
    this.ctx = this.$refs.canvas.getContext("2d");
    this.onResize = () => this.resize();
    this.onVisibility = () => { document.hidden ? this.stop() : this.start(); };
    window.addEventListener("resize", this.onResize);
    document.addEventListener("visibilitychange", this.onVisibility);
    this.resize();
    if (REDUCED_MOTION) this.draw(0);
    else this.start();
  },
  unmounted() {
    this.stop();
    window.removeEventListener("resize", this.onResize);
    document.removeEventListener("visibilitychange", this.onVisibility);
  },
  methods: {
    resize() {
      const canvas = this.$refs.canvas;
      this.w = window.innerWidth;
      this.h = window.innerHeight;
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      canvas.width = this.w * dpr;
      canvas.height = this.h * dpr;
      canvas.style.width = this.w + "px";
      canvas.style.height = this.h + "px";
      this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      this.seed();
    },
    seed() {
      const count = Math.min(64, Math.max(24, Math.floor((this.w * this.h) / 26000)));
      this.nodes = Array.from({ length: count }, () => ({
        x: Math.random() * this.w,
        y: Math.random() * this.h,
        vx: (Math.random() - 0.5) * 0.12,
        vy: (Math.random() - 0.5) * 0.12,
        r: 1 + Math.random() * 1.6,
        c: COLORS[Math.floor(Math.random() * COLORS.length)],
        phase: Math.random() * Math.PI * 2,
      }));
    },
    draw(t) {
      const ctx = this.ctx;
      ctx.clearRect(0, 0, this.w, this.h);
      const nodes = this.nodes;
      // filaments between nearby nodes
      for (let i = 0; i < nodes.length; i++) {
        for (let j = i + 1; j < nodes.length; j++) {
          const a = nodes[i], b = nodes[j];
          const dx = a.x - b.x, dy = a.y - b.y;
          const d2 = dx * dx + dy * dy;
          if (d2 > LINK_DIST * LINK_DIST) continue;
          const alpha = (1 - Math.sqrt(d2) / LINK_DIST) * 0.07;
          ctx.strokeStyle = "rgba(" + a.c[0] + "," + a.c[1] + "," + a.c[2] + "," + alpha.toFixed(3) + ")";
          ctx.lineWidth = 0.7;
          ctx.beginPath();
          ctx.moveTo(a.x, a.y);
          ctx.lineTo(b.x, b.y);
          ctx.stroke();
        }
      }
      // nodes with a soft breathing glow
      for (const n of nodes) {
        const breathe = 0.55 + 0.45 * Math.sin(t / 2400 + n.phase);
        const rgb = n.c[0] + "," + n.c[1] + "," + n.c[2];
        ctx.fillStyle = "rgba(" + rgb + "," + (0.10 * breathe).toFixed(3) + ")";
        ctx.beginPath();
        ctx.arc(n.x, n.y, n.r * 3.2, 0, Math.PI * 2);
        ctx.fill();
        ctx.fillStyle = "rgba(" + rgb + "," + (0.5 * breathe).toFixed(3) + ")";
        ctx.beginPath();
        ctx.arc(n.x, n.y, n.r, 0, Math.PI * 2);
        ctx.fill();
      }
    },
    step(t) {
      this.raf = requestAnimationFrame(ts => this.step(ts));
      if (t - this.last < 33) return; // ~30 fps
      this.last = t;
      for (const n of this.nodes) {
        n.x += n.vx;
        n.y += n.vy;
        if (n.x < -20) n.x = this.w + 20; else if (n.x > this.w + 20) n.x = -20;
        if (n.y < -20) n.y = this.h + 20; else if (n.y > this.h + 20) n.y = -20;
      }
      this.draw(t);
    },
    start() {
      if (this.raf !== null || REDUCED_MOTION) return;
      this.raf = requestAnimationFrame(t => this.step(t));
    },
    stop() {
      if (this.raf !== null) { cancelAnimationFrame(this.raf); this.raf = null; }
    },
  },
  template: '<canvas id="mycelium" ref="canvas"></canvas>',
};
