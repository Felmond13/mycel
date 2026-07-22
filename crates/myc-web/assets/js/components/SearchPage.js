// Reverse query ("which"): find every environment containing a path
// fragment or a BLAKE3 hash prefix.

import { api } from "../api.js";
import { fmtBytes, shortRef } from "../format.js";

export default {
  name: "SearchPage",
  data() { return { q: "", result: null, error: "", busy: false }; },
  methods: {
    fmtBytes, shortRef,
    async search() {
      const q = this.q.trim();
      if (!q) return;
      this.busy = true;
      this.error = "";
      try { this.result = await api.get("/api/which?q=" + encodeURIComponent(q)); }
      catch (e) { this.error = e.message; this.result = null; }
      this.busy = false;
    },
  },
  template: `
  <section>
    <h1 class="title">Find <b>a file</b></h1>
    <div class="sub">Which apps in your library contain this exact file? Search by path fragment (e.g. \u201cbusybox\u201d)
      or by BLAKE3 hash prefix (6+ hex characters) — the vulnerable-OpenSSL question, answered instantly.</div>

    <div class="card">
      <div class="row">
        <input type="text" v-model="q" @keydown.enter="search"
               placeholder="path fragment or hash prefix… e.g. /bin/busybox" style="flex:1;min-width:220px" spellcheck="false">
        <button class="btn primary" :disabled="busy" @click="search">Search</button>
      </div>
    </div>

    <empty-state v-if="error" :title="error"></empty-state>
    <template v-else-if="result">
      <empty-state v-if="!result.hits.length" title="No matches">
        Nothing in the store contains \u201c{{ result.query }}\u201d.
      </empty-state>
      <div v-else class="card tight">
        <table>
          <thead><tr><th>App</th><th>Path</th><th>Matched by</th><th class="num">Size</th></tr></thead>
          <tbody>
            <tr v-for="(h, i) in result.hits.slice(0, 500)" :key="i">
              <td><a :href="'#env/' + encodeURIComponent(h.manifest_id)">{{ shortRef(h.manifest_name) }}</a></td>
              <td class="mono">{{ h.path }}</td>
              <td><span class="pill" :class="h.matched === 'hash' ? 'teal' : 'gray'">{{ h.matched }}</span></td>
              <td class="num">{{ fmtBytes(h.size) }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </template>
  </section>`,
};
