// Identicon-style avatar: a colored initial with a hue derived
// deterministically from the reference name (same app = same color).
// <ref-avatar :reference="'nginxinc/nginx-unprivileged:latest'" size="34">

import { parseRef, refHue } from "../format.js";

export default {
  name: "RefAvatar",
  props: {
    reference: { type: String, required: true },
    size: { type: [String, Number], default: 34 },
  },
  computed: {
    initial() {
      const n = parseRef(this.reference).shortName || "?";
      return n.replace(/^myc1-/, "").charAt(0).toUpperCase();
    },
    css() {
      const h = refHue(this.reference);
      const s = Number(this.size);
      return {
        width: s + "px", height: s + "px",
        fontSize: Math.round(s * 0.42) + "px",
        background: "linear-gradient(135deg, hsla(" + h + ",70%,60%,.20), hsla(" + ((h + 40) % 360) + ",70%,55%,.12))",
        borderColor: "hsla(" + h + ",70%,65%,.35)",
        color: "hsl(" + h + ",75%,72%)",
      };
    },
  },
  template: '<span class="avatar" :style="css" :title="reference">{{ initial }}</span>',
};
