// Full-height slide-over showing a container's log, tail-polled by ui.js.
// Header carries the app avatar + name, the follow toggle (auto-scroll)
// and the download button; under it, an incremental client-side filter.
// The full log is always kept in memory — the filter only narrows what
// is displayed.

import { UI, closeLogs } from "../ui.js";
import { S } from "../store.js";
import { historyFor, fmtCpu } from "../metrics.js";
import { fmtBytes } from "../format.js";

export default {
  name: "LogsPanel",
  data() { return { query: "" }; },
  computed: {
    logs() { return UI.logs; },
    statusClass() {
      if (this.logs.running) return "green";
      return this.logs.exit === 0 ? "gray" : "red";
    },
    /** Live container object behind the drawer (for current CPU/RAM). */
    live() { return S.containers.find(c => c.id === this.logs.cid && c.running); },
    hist() { return historyFor(this.logs.cid); },
    /** Filtered lines (incremental client-side search, case-insensitive). */
    lines() { return this.logs.text ? this.logs.text.split("\n") : []; },
    filtered() {
      const q = this.query.trim().toLowerCase();
      if (!q) return null;
      return this.lines.filter(l => l.toLowerCase().includes(q));
    },
    totalLines() { return this.lines.length; },
  },
  watch: {
    // Opening another container's logs resets the filter.
    "logs.cid"() { this.query = ""; },
  },
  methods: {
    closeLogs, fmtCpu, fmtBytes,
    toggleFollow() { UI.logs.follow = !UI.logs.follow; },
    /** Download the complete log (unfiltered) as a .log blob. */
    download() {
      const name = (this.logs.title || "mycel")
        .toLowerCase().replace(/[^a-z0-9._-]+/g, "-").replace(/^-+|-+$/g, "") || "mycel";
      const blob = new Blob([this.logs.text || ""], { type: "text/plain;charset=utf-8" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = name + ".log";
      a.click();
      setTimeout(() => URL.revokeObjectURL(url), 5000);
    },
  },
  template: `
  <Transition name="drawer">
    <div v-if="logs.open" class="drawer-wrap">
      <div class="drawer-back" @click="closeLogs"></div>
      <div class="drawer">
        <div class="drawer-head">
          <ref-avatar v-if="logs.reference" :reference="logs.reference" size="30"></ref-avatar>
          <div class="who">
            <b>{{ logs.title }}</b>
            <ref-name v-if="logs.reference" :reference="logs.reference" :bold="false"></ref-name>
          </div>
          <span class="pill" :class="statusClass">{{ logs.status }}</span>
          <span class="spacer"></span>
          <button class="btn sm" :class="logs.follow ? 'success' : 'ghost'" @click="toggleFollow"
                  :title="logs.follow ? 'New output scrolls into view' : 'View stays where you scrolled'">
            {{ logs.follow ? '● Following' : '○ Follow' }}
          </button>
          <button class="btn sm ghost" @click="download" title="Télécharger le journal complet (.log)">
            <svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M8 2v8M4.5 6.5 8 10l3.5-3.5"/><path d="M2.5 12.5h11"/></svg>
            Télécharger
          </button>
          <button class="btn sm ghost" @click="closeLogs">Close</button>
        </div>
        <div class="drawer-tools">
          <input type="text" v-model="query" placeholder="Filtrer les lignes…"
                 autocomplete="off" spellcheck="false" class="log-filter">
          <span v-if="filtered" class="log-count">{{ filtered.length }} / {{ totalLines }} lignes</span>
          <button v-if="query" class="iconbtn" title="Effacer le filtre" @click="query = ''">
            <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M4 4l8 8M12 4l-8 8"/></svg>
          </button>
        </div>
        <div v-if="live && hist.cpu.length > 1" class="drawer-sparks">
          <div class="spark-box">
            <div class="k">CPU <span class="mono">{{ fmtCpu(live.cpu_percent) }}</span></div>
            <spark-line :values="hist.cpu" :max="100" color="var(--teal)" :height="24"></spark-line>
          </div>
          <div class="spark-box">
            <div class="k">RAM <span class="mono">{{ fmtBytes(live.memory_bytes) }}</span></div>
            <spark-line :values="hist.mem" color="var(--violet)" :height="24"></spark-line>
          </div>
        </div>
        <pre v-if="!query" class="drawer-body">{{ logs.text || '(no output yet)' }}</pre>
        <pre v-else-if="filtered && filtered.length" class="drawer-body">{{ filtered.join("\\n") }}</pre>
        <div v-else class="drawer-body" style="color:var(--faint)">
          aucune ligne ne correspond à « {{ query }} »
        </div>
      </div>
    </div>
  </Transition>`,
};
