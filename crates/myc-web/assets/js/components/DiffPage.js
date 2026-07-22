// Compare two library apps: elegant A/B pickers, a summary strip, and the
// changes grouped by kind with color coding.

import { S } from "../store.js";
import { api } from "../api.js";
import { fmtBytes, shortRef } from "../format.js";

export default {
  name: "DiffPage",
  data() { return { a: "", b: "", result: null, error: "", busy: false }; },
  computed: {
    envs() { return S.envs; },
    groups() {
      if (!this.result) return [];
      const by = { added: [], removed: [], modified: [], meta_changed: [] };
      for (const c of this.result.report.changes) by[c.change].push(c);
      return [
        { kind: "added", label: "Added", sign: "+", cls: "tc-green", items: by.added },
        { kind: "removed", label: "Removed", sign: "−", cls: "tc-red", items: by.removed },
        { kind: "modified", label: "Modified", sign: "~", cls: "tc-amber", items: by.modified },
        { kind: "meta_changed", label: "Metadata only", sign: "≈", cls: "tc-cyan", items: by.meta_changed },
      ].filter(g => g.items.length);
    },
    total() {
      const r = this.result && this.result.report;
      return r ? r.added + r.removed + r.modified + r.meta_changed : 0;
    },
  },
  watch: {
    envs: {
      immediate: true,
      handler(list) {
        if (!this.a && list.length) this.a = list[0].id;
        if (!this.b && list.length > 1) this.b = list[1].id;
        else if (!this.b && list.length) this.b = list[0].id;
      },
    },
  },
  methods: {
    fmtBytes, shortRef,
    async compare() {
      if (!this.a || !this.b) return;
      this.busy = true;
      this.error = "";
      this.result = null;
      try {
        this.result = await api.get("/api/diff?a=" + encodeURIComponent(this.a) + "&b=" + encodeURIComponent(this.b));
      } catch (e) { this.error = e.message; }
      this.busy = false;
    },
  },
  template: `
  <section>
    <h1 class="title">Compare <b>two apps</b></h1>
    <div class="sub">Exact file-by-file comparison between two apps in your library — Mycel knows every file, so nothing is approximate.</div>

    <empty-state v-if="!envs.length" title="Nothing to compare yet">
      Get two apps first — e.g. alpine:3.20 and alpine:3.21.
      <div><a href="#ingest"><button class="btn primary">Get one →</button></a></div>
    </empty-state>
    <template v-else>
      <div class="card">
        <div class="diff-pick">
          <div class="side">
            <span class="label">From (A)</span>
            <select v-model="a">
              <option v-for="e in envs" :key="e.id" :value="e.id">{{ shortRef(e.name) }}</option>
            </select>
          </div>
          <div class="arrow" aria-hidden="true">
            <svg viewBox="0 0 24 16" width="30" height="20" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M2 8h18M15 3l5 5-5 5"/></svg>
          </div>
          <div class="side">
            <span class="label">To (B)</span>
            <select v-model="b">
              <option v-for="e in envs" :key="e.id" :value="e.id">{{ shortRef(e.name) }}</option>
            </select>
          </div>
        </div>
        <div class="row" style="margin-top:16px">
          <button class="btn primary" :disabled="busy" @click="compare">
            <span v-if="busy" class="spinner"></span>{{ busy ? 'Comparing…' : 'Compare' }}
          </button>
        </div>
      </div>

      <empty-state v-if="error" :title="error"></empty-state>
      <template v-else-if="result">
        <empty-state v-if="!total" title="Identical">
          {{ shortRef(result.a.name) }} and {{ shortRef(result.b.name) }} contain exactly the same files.
        </empty-state>
        <template v-else>
          <div class="card">
            <b>{{ shortRef(result.b.name) }}</b> differs from <b>{{ shortRef(result.a.name) }}</b> by {{ total }} file{{ total > 1 ? 's' : '' }},
            <b style="color:var(--teal)">{{ fmtBytes(result.report.new_bytes) }}</b> of genuinely new content.
            <div class="row" style="margin-top:12px">
              <span class="pill green">+ {{ result.report.added }} added</span>
              <span class="pill red">− {{ result.report.removed }} removed</span>
              <span class="pill amber">~ {{ result.report.modified }} modified</span>
              <span class="pill teal">≈ {{ result.report.meta_changed }} metadata</span>
              <span class="pill gray">= {{ result.report.unchanged }} unchanged</span>
            </div>
          </div>
          <div class="card tight" style="max-height:560px;overflow-y:auto">
            <div v-for="g in groups" :key="g.kind" class="diff-group">
              <div class="ghead" :class="g.cls">{{ g.sign }} {{ g.label }} <span class="pill gray">{{ g.items.length }}</span></div>
              <div v-for="c in g.items" :key="c.path" class="diff-row">
                <span class="p" :class="g.cls">{{ c.path }}</span>
                <span class="d">{{ c.detail }}</span>
                <span class="s">{{ c.change === 'removed' ? fmtBytes(c.size_a) : fmtBytes(c.size_b) }}</span>
              </div>
            </div>
          </div>
        </template>
      </template>
    </template>
  </section>`,
};
