// Store page: the "saved by deduplication" headline, store stats, animated
// shared-vs-unique bars per app, storage (dedup) modes with their measured
// footprint, and cleanup (garbage collection).

import { S, refreshState, refreshStats } from "../store.js";
import { api } from "../api.js";
import { toast, confirmDialog } from "../ui.js";
import { fmtBytes } from "../format.js";

// Known store modes → French label + measured key from /api/store/modes.
const MODE_LABELS = {
  "v1": "v1 — historique",
  "transfer": "transfer",
  "lean": "lean",
  "packs-only": "packs-only",
};

export default {
  name: "StorePage",
  data() {
    return {
      gcBusy: false, volumes: null, volBusy: {}, copiedVol: "",
      modes: null, modesError: "", modeBusy: false, modePending: "", modeErr: "",
    };
  },
  computed: {
    st() { return S.stats; },
    savedPct() {
      const s = S.stats;
      if (!s || !s.logical_bytes) return 0;
      return Math.round(100 * s.saved_bytes / s.logical_bytes);
    },
    /* /api/store/modes rows: label + measured footprint (Mio) + current. */
    modeRows() {
      const m = this.modes;
      if (!m) return [];
      const list = Array.isArray(m.available) && m.available.length
        ? m.available : Object.keys(MODE_LABELS);
      return list.map(id => ({
        id,
        label: MODE_LABELS[id] || id,
        mib: this.measuredMib(id),
        current: m.current === id,
      }));
    },
    measuredSource() {
      const me = this.modes && this.modes.measured;
      return (me && me.source) || "";
    },
    /* packs-only gain vs v1, from the server's own measurements
       (e.g. "−46,6 %"). Null when the numbers are not available. */
    packsGain() {
      const me = this.modes && this.modes.measured;
      if (!me || !(Number(me.v1_mib) > 0)) return null;
      const p = Number(me.packs_only_mib);
      if (!Number.isFinite(p) || p < 0) return null;
      const pct = Math.round(Math.abs(100 * (me.v1_mib - p) / me.v1_mib) * 10) / 10;
      const sign = p <= me.v1_mib ? "−" : "+";
      return sign + String(pct).replace(".", ",") + " %";
    },
    /* Physical/logical width for the chunk-sharing bars. */
    physicalPct() {
      const s = S.stats;
      if (!s || !s.logical_bytes || !s.physical_bytes) return 0;
      const p = 100 * s.physical_bytes / s.logical_bytes;
      return p <= 0 ? 0 : Math.max(1.5, Math.min(100, p));
    },
  },
  mounted() { refreshStats(); this.loadVolumes(); this.loadModes(); },
  methods: {
    fmtBytes,
    /* ---- storage (dedup) modes ---- */
    async loadModes() {
      try { this.modes = await api.get("/api/store/modes"); this.modesError = ""; }
      catch (e) { this.modes = null; this.modesError = e.message; }
    },
    measuredMib(id) {
      const me = this.modes && this.modes.measured;
      if (!me) return null;
      return me[id.replace(/-/g, "_") + "_mib"];
    },
    fmtMib(v) {
      if (v === null || v === undefined || v === "" || !Number.isFinite(Number(v))) return "—";
      return (Math.round(Number(v) * 10) / 10).toString().replace(".", ",") + " Mio";
    },
    async useMode(r) {
      if (this.modeBusy || r.current) return;
      const ok = await confirmDialog("Passer le stockage en mode « " + r.label + " » ?",
        "Le magasin suivra ce mode de partage pour ses prochaines écritures. Vos apps installées et leurs données ne sont pas supprimées.", "Changer de mode");
      if (!ok) return;
      this.modeBusy = true;
      this.modePending = r.id;
      this.modeErr = "";
      try {
        await api.put("/api/store/modes", { mode: r.id });
        toast("Mode de stockage : " + r.label);
        this.loadModes();
      } catch (e) {
        // 409 (mode refused, e.g. while apps are running) → the server
        // explains why; show it inline and as a toast.
        this.modeErr = e.message;
        toast(e.message, true);
        this.loadModes();
      }
      this.modeBusy = false;
      this.modePending = "";
    },
    /* ---- volumes ---- */
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

      <div class="section-title">Modes de stockage</div>
      <div class="sub" style="margin-bottom:14px">
        Jusqu'à quel point les fichiers identiques sont partagés entre vos apps — plus le mode partage, moins le disque est occupé.
      </div>
      <div v-if="modesError" class="dim-banner">
        Informations sur les modes indisponibles<span v-if="modesError"> — {{ modesError }}</span>.
        Mettez à jour le serveur Mycel pour piloter la déduplication depuis le tableau de bord.
      </div>
      <div v-else-if="modes" class="card">
        <table>
          <thead>
            <tr>
              <th>Mode</th>
              <th class="num">Occupation mesurée</th>
              <th class="ctr">État</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="r in modeRows" :key="r.id" :class="{ cur: r.current }">
              <td><b>{{ r.label }}</b></td>
              <td class="num mono">{{ fmtMib(r.mib) }}</td>
              <td class="ctr">
                <span v-if="r.current" class="pill teal">actif</span>
                <button v-else class="btn sm ghost" :disabled="modeBusy" @click="useMode(r)">
                  <span v-if="modeBusy && modePending === r.id" class="spinner"></span>Utiliser
                </button>
              </td>
            </tr>
          </tbody>
        </table>
        <div v-if="modeErr" style="color:var(--red);font-size:13px;margin-top:10px">{{ modeErr }}</div>
        <div v-if="modes.note" style="color:var(--muted);font-size:12.5px;margin-top:10px">{{ modes.note }}</div>
        <div v-if="measuredSource" style="color:var(--faint);font-size:12px;margin-top:8px">
          mesures : <span class="mono">{{ measuredSource }}</span>
        </div>
      </div>

      <div class="card">
        <div style="font-weight:650;margin-bottom:12px">Partage par morceaux (chunks)</div>
        <div class="chunkrow">
          <span class="k">logique — la somme de vos apps</span>
          <div class="bar"><i class="shared" style="width:100%"></i></div>
          <span class="v mono">{{ fmtBytes(st.logical_bytes) }}</span>
        </div>
        <div class="chunkrow">
          <span class="k">physique — réellement sur le disque</span>
          <div class="bar"><i class="unique" :style="{ width: physicalPct + '%' }"></i></div>
          <span class="v mono">{{ fmtBytes(st.physical_bytes) }}</span>
        </div>
        <div v-if="packsGain" class="hint-line" style="margin-top:14px">
          Le mode <b>packs-only</b> occupe <b>{{ packsGain }}</b> par rapport au mode v1
          sur ce magasin<span v-if="measuredSource"> — mesure <span class="mono">{{ measuredSource }}</span></span>.
        </div>
        <div v-else-if="savedPct > 0" class="hint-line" style="margin-top:14px">
          <b>{{ savedPct }} %</b> de ce que vous avez installé n'a jamais été écrit deux fois sur le disque.
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
