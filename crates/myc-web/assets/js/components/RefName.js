// Human display of a reference: muted registry/namespace prefix (hidden for
// docker.io/library), the short name, and the tag as a small badge.
// <ref-name reference="docker.io/library/alpine:3.20">  ->  alpine [3.20]

import { parseRef, refPrefix } from "../format.js";

export default {
  name: "RefName",
  props: {
    reference: { type: String, required: true },
    bold: { type: Boolean, default: true },
  },
  computed: {
    p() { return parseRef(this.reference); },
    prefix() { return refPrefix(this.p); },
    badge() {
      if (this.p.digest) return this.p.digest.slice(0, 15) + "…";
      return this.p.tag;
    },
  },
  template: `
  <span class="refname" :title="reference">
    <span v-if="prefix" class="pfx">{{ prefix }}/</span><span class="nm" :class="{ b: bold }">{{ p.shortName }}</span><span v-if="badge" class="tagb">{{ badge }}</span>
  </span>`,
};
