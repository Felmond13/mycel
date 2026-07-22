// Store page: the "saved by deduplication" headline, store stats, animated
// shared-vs-unique bars per app, and cleanup (garbage collection).

import { S, refreshState, refreshStats } from "../store.js";
import { api } from "../api.js";
import { toast, confirmDialog } from "../ui.js";
import { fmtBytes } from "../format.js";

export default {
  name: "StorePage",
  data() { return { gcBusy: false, volumes: null, volBusy: {}, copiedVol: "" }; },
  computed: {
    st() { return S.stats; },
    savedPct() {
      const s = S.stats;
      if (!s || !s.logical_bytes) return 0;
      return Math.round(100 * s.saved_bytes / s.logical_bytes);
    },
  },
  mounted() { refreshStats(); this.loadVolumes(); },
  methods: {
    fmtBytes,
    async loadVolumes() {
      try { this.volumes = (await api.get("/api/volumes")).volumes; }
      catch (_) { this.volumes = []; }
    },
    async copyVolPath(v) {
      const path = v.windows_path || v.path;
      try { await navigator.clipboard.writeText(path); } catch (_) { /* best effort */ }
      this.copiedVol = v.name;
      setTimeout(() => { this.copiedVol = ""; }, 1600);
    },
    async removeVolume(v) {
      const ok = await confirmDialog("Delete the data of " + v.name + "?",
        "Everything this app saved (" + fmtBytes(v.size_bytes) + ") is permanently deleted. The app itself stays installed.", "Delete data");
      if (!ok) return;
      this.volBusy[v.name] = true;
      try {
        await api.del("/api/volumes/" + encodeURIComponent(v.name));
        toast("Deleted " + v.name + " (" + fmtBytes(v.size_bytes) + " freed)");
        this.loadVolumes();
      } catch (e) { toast(e.message, true); }
      delete this.volBusy[v.name];
    },
    async gc() {
      const ok = await confirmDialog("Clean up unused files?",
        "Deletes every file nothing in your library uses anymore. Pinned apps are always safe.", "Clean up");
      if (!ok) return;
      this.gcBusy = true;
      try {
        const r = await api.post("/api/gc");
        toast("Cleanup done: " + r.deleted + " files deleted, " + fmtBytes(r.freed_bytes) + " freed");
        refreshStats(); refreshState();
      } catch (e) { toast(e.message, true); }
      this.gcBusy = false;
    },
  },
  template: `
  <section>
    <h1 class="title">The <b>store</b></h1>
    <div class="sub">One content-addressed pool for your whole library. Identical files across apps occupy disk exactly once.</div>

    <skeleton-block v-if="!st" h="110px" :n="2"></skeleton-block>
    <template v-else>
      <div class="card headline-stat">
        <div class="big"><anim-num :value="st.saved_bytes" fmt="bytes"></anim-num></div>
        <div class="cap">saved by deduplication<span v-if="savedPct > 0"> — {{ savedPct }}% of what you installed never hit the disk twice</span></div>
      </div>

      <div class="stat-grid">
        <stat-card :value="st.environments" label="apps in library"></stat-card>
        <stat-card :value="st.blobs" label="unique files"></stat-card>
        <stat-card :value="st.physical_bytes" fmt="bytes" label="physical (on disk)"></stat-card>
        <stat-card :value="st.logical_bytes" fmt="bytes" label="logical (sum of apps)"></stat-card>
        <stat-card :value="st.dedup_ratio" fmt="ratio" label="dedup ratio" variant="grad"></stat-card>
      </div>

      <div class="section-title">Sharing per app</div>
      <div class="sub" style="margin-bottom:14px">How much of each app is shared with the rest of your library — the reason getting one more app costs almost nothing.</div>
      <div class="card">
        <template v-if="st.per_environment.length">
          <div v-for="d in st.per_environment" :key="d.id" class="share-row">
            <div class="row spread" style="margin-bottom:6px">
              <span class="row" style="gap:9px"><ref-avatar :reference="d.name" size="24"></ref-avatar><ref-name :reference="d.name"></ref-name></span>
              <span class="share-nums">
                {{ Math.round(d.shared_ratio * 100) }}% shared · {{ fmtBytes(d.unique_bytes) }} unique
              </span>
            </div>
            <div class="bar">
              <i class="shared" :style="{ width: (d.shared_ratio * 100) + '%' }" :title="'shared: ' + fmtBytes(d.shared_bytes)"></i>
              <i class="unique" :style="{ width: ((1 - d.shared_ratio) * 100) + '%' }" :title="'unique: ' + fmtBytes(d.unique_bytes)"></i>
            </div>
          </div>
          <div style="color:var(--muted);font-size:12.5px">
            <span style="color:var(--teal)">■</span> shared with other apps&nbsp;&nbsp;
            <span style="color:#2a3450">■</span> unique to this one
          </div>
        </template>
        <div v-else style="color:var(--muted);font-size:13px">
          Get at least one app to see sharing statistics. Two related versions
          (e.g. alpine:3.20 and alpine:3.21) make a great demo.
        </div>
      </div>

      <template v-if="volumes && volumes.length">
        <div class="section-title">App data</div>
        <div class="sub" style="margin-bottom:14px">What your apps have saved. This data survives stops and restarts — delete it only to start an app from scratch.</div>
        <div class="card">
          <div v-for="v in volumes" :key="v.name" class="row spread" style="padding:8px 0;border-bottom:1px solid rgba(255,255,255,.05)">
            <div style="min-width:0">
              <div class="row" style="gap:8px">
                <b class="mono" style="font-size:13px">{{ v.name }}</b>
                <span v-if="v.in_use" class="pill green" :title="'used right now by ' + v.used_by.join(', ')">in use</span>
              </div>
              <div class="mono" style="color:var(--faint);font-size:11px;word-break:break-all" :title="v.windows_path || ''">{{ v.path }}</div>
            </div>
            <div class="row" style="gap:8px;flex-shrink:0">
              <span class="mono" style="color:var(--muted);font-size:12.5px">{{ fmtBytes(v.size_bytes) }}</span>
              <button class="btn sm ghost" :title="v.windows_path ? 'Copies the Windows path — paste it into Explorer' : 'Copies the folder path'"
                      @click="copyVolPath(v)">{{ copiedVol === v.name ? 'Copied ✓' : 'Open folder' }}</button>
              <button class="btn sm danger" :disabled="v.in_use || volBusy[v.name]"
                      :title="v.in_use ? 'Stop ' + v.used_by.join(', ') + ' first' : 'Delete this app data forever'"
                      @click="removeVolume(v)">Delete</button>
            </div>
          </div>
        </div>
      </template>

      <div class="row" style="margin-top:20px">
        <button class="btn ghost" :disabled="gcBusy" @click="gc">
          <span v-if="gcBusy" class="spinner"></span>{{ gcBusy ? 'Cleaning…' : 'Clean up unused files' }}
        </button>
        <span style="color:var(--muted);font-size:13px">deletes files nothing in your library uses</span>
      </div>
    </template>
  </section>`,
};
