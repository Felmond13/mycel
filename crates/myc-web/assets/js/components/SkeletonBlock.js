// Shimmering placeholder rows shown while a page loads its data.
// <skeleton-block h="76px" :n="3">

export default {
  name: "SkeletonBlock",
  props: {
    h: { type: String, default: "72px" },
    n: { type: Number, default: 3 },
  },
  template: `
  <div>
    <div v-for="i in n" :key="i" class="skel" :style="{ height: h, marginBottom: '12px' }"></div>
  </div>`,
};
