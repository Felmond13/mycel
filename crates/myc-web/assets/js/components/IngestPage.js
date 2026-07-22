// Get-any-app page: fetch software from any registry by name, with live
// progress. The result in the library is Mycel-native: files, not images.

import { S, refreshState, refreshStats, installImageAndWait } from "../store.js";
import { api } from "../api.js";
import { toast } from "../ui.js";
import { fmtBytes, fmtDur, shortRef } from "../format.js";

export default {
  name: "IngestPage",
  data() {
    return {
      input: "",
      busy: false,
      prog: null, // {pct|null, msg, files, bytes}
      done: null, // final job object
      fail: "",
      jobs: [],   // recent ingest jobs (server-side, this server session)
      chips: ["alpine:3.20", "alpine:3.21", "busybox:latest", "debian:12-slim", "redis:7-alpine"],
    };
  },
  mounted() { this.loadJobs(); },
  methods: {
    fmtBytes, shortRef,
    agoText(j) { return fmtDur(S.now - j.created_at) + " ago"; },
    async loadJobs() {
      try { this.jobs = (await api.get("/api/jobs")).jobs; } catch (_) { /* history is optional */ }
    },
    progressText(j) {
      if (j.layers_total) return "downloading " + j.layers_done + "/" + j.layers_total;
      return "contacting the registry…";
    },
    async start(name) {
      name = (name || this.input).trim();
      if (!name) { toast("Type an app name first, e.g. alpine:3.20", true); return; }
      this.input = name;
      this.busy = true;
      this.done = null;
      this.fail = "";
      this.prog = { pct: null, msg: "contacting the registry…", files: 0, bytes: 0 };
      try {
        this.done = await installImageAndWait(name, j => {
          this.prog = {
            pct: j.layers_total ? Math.round(100 * j.layers_done / j.layers_total) : null,
            msg: this.progressText(j), files: j.files, bytes: j.bytes_added,
          };
        });
        this.prog = null;
        toast(shortRef(name) + " added to your library");
        refreshState(); refreshStats();
      } catch (e) {
        this.fail = e.message;
        this.prog = null;
      }
      this.busy = false;
      this.loadJobs();
    },
  },
  template: `
  <section>
    <h1 class="title">Get <b>any app</b></h1>
    <div class="sub">Fetch from any registry — Docker Hub, GHCR, Quay… — stored the Mycel way: files, not images.</div>

    <div class="card">
      <div class="row">
        <input type="text" v-model="input" @keydown.enter="start()"
               placeholder="app name — e.g. alpine:3.20 or ghcr.io/owner/app" style="flex:1;min-width:220px" spellcheck="false">
        <button class="btn primary" :disabled="busy" @click="start()">
          <span v-if="busy" class="spinner"></span>{{ busy ? 'Fetching…' : 'Get it' }}
        </button>
      </div>
      <div style="margin-top:14px">
        <span style="color:var(--muted);font-size:13px;margin-right:8px">Try:</span>
        <span v-for="c in chips" :key="c" class="chip" @click="start(c)">{{ c }}</span>
      </div>

      <template v-if="prog">
        <div class="prog" :class="{ indet: prog.pct === null }">
          <i :style="{ width: (prog.pct === null ? 30 : prog.pct) + '%' }"></i>
        </div>
        <div style="color:var(--muted);font-size:13px">
          {{ prog.msg }} — {{ prog.files }} files, {{ fmtBytes(prog.bytes) }} new so far
        </div>
      </template>
      <template v-if="done">
        <div class="prog"><i style="width:100%;background:var(--green);box-shadow:0 0 12px rgba(74,222,128,.5)"></i></div>
        <div style="color:var(--green);font-size:13px">
          ✓ {{ shortRef(done.image) }} is in your library — {{ done.files }} files,
          {{ fmtBytes(done.logical_size) }} in total, only {{ fmtBytes(done.bytes_added) }} of it new on disk
          <a :href="'#env/' + encodeURIComponent(done.manifest_id)">inspect →</a>
        </div>
      </template>
      <div v-if="fail" style="color:var(--red);font-size:13px;margin-top:12px">✗ {{ fail }}</div>
    </div>

    <template v-if="jobs.length">
      <div class="section-title">Recent fetches</div>
      <div class="card tight">
        <table>
          <thead><tr><th>App</th><th>Status</th><th class="num">Files</th><th class="num">New on disk</th><th class="num">When</th></tr></thead>
          <tbody>
            <tr v-for="j in jobs" :key="j.id">
              <td><b>{{ shortRef(j.image) }}</b></td>
              <td>
                <span v-if="j.status === 'done'" class="pill green">done</span>
                <span v-else-if="j.status === 'failed'" class="pill red" :title="j.message">failed</span>
                <span v-else class="pill amber">running</span>
              </td>
              <td class="num">{{ j.files || '' }}</td>
              <td class="num">{{ j.bytes_added ? fmtBytes(j.bytes_added) : '' }}</td>
              <td class="num" style="color:var(--faint)">{{ agoText(j) }}</td>
            </tr>
          </tbody>
        </table>
      </div>
      <div class="sub" style="margin-top:8px">History covers this dashboard session — it resets when the server restarts.</div>
    </template>

    <div class="hint-line">
      <b>How it works</b> — Mycel downloads the app once, breaks it into individual files, and keeps each
      unique file exactly once. Everything you get is deduplicated, verifiable, and works offline.
    </div>
  </section>`,
};
