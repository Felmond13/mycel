// One library app: stats, run commands, technical details (manifest id,
// canonical name), SBOM, file browser.

import { api } from "../api.js";
import { toast } from "../ui.js";
import { fmtBytes } from "../format.js";

export default {
  name: "LibraryDetail",
  props: { arg: { type: String, required: true } },
  data() {
    // `filterInput` is what the user types; `filter` is applied after a
    // short debounce so 7k+ file lists don't re-render on every keystroke.
    return { d: null, error: "", sbom: null, sbomStatus: "Loading package list…", filter: "", filterInput: "", filterTimer: null };
  },
  watch: {
    filterInput(v) {
      clearTimeout(this.filterTimer);
      this.filterTimer = setTimeout(() => { this.filter = v; }, 150);
    },
  },
  unmounted() { clearTimeout(this.filterTimer); },
  computed: {
    cfgRows() {
      if (!this.d) return [];
      const cfg = this.d.config;
      return [
        ["entrypoint", cfg.entrypoint.length ? JSON.stringify(cfg.entrypoint) : ""],
        ["cmd", cfg.cmd.length ? JSON.stringify(cfg.cmd) : ""],
        ["workdir", cfg.workdir], ["user", cfg.user],
      ].filter(r => r[1]);
    },
    filtered() {
      if (!this.d) return [];
      const f = this.filter.trim().toLowerCase();
      return f ? this.d.entries.filter(e => e.path.toLowerCase().includes(f)) : this.d.entries;
    },
    shown() { return this.filtered.slice(0, 800); },
  },
  async mounted() {
    try { this.d = await api.get("/api/envs/" + encodeURIComponent(this.arg)); }
    catch (e) { this.error = e.message; return; }
    try {
      const sbom = await api.get("/api/envs/" + encodeURIComponent(this.d.id) + "/sbom");
      this.sbom = sbom;
      this.sbomStatus = sbom.packages.length
        ? sbom.packages.length + " packages found via " + sbom.package_source + " database"
        : "No package database found — the SBOM still lists every file with its content hash.";
    } catch (e) { this.sbomStatus = "SBOM unavailable: " + e.message; }
  },
  methods: {
    fmtBytes,
    fileLabel(e) { return e.kind === "symlink" ? e.path + " → " + e.target : e.path; },
    fileKind(e) { return { file: "f", dir: "d", symlink: "l" }[e.kind] || "?"; },
    copy(text) {
      navigator.clipboard.writeText(text).then(
        () => toast("Copied to clipboard"),
        () => toast("Copy failed", true));
    },
    async run() {
      try {
        await api.post("/api/containers", { reference: this.d.id });
        toast("Started — watch it under \u201cRunning\u201d");
        location.hash = "running";
      } catch (e) { toast(e.message, true); }
    },
    downloadSbom() {
      const blob = new Blob([JSON.stringify(this.sbom, null, 2)], { type: "application/json" });
      const a = document.createElement("a");
      a.href = URL.createObjectURL(blob);
      a.download = this.d.name.replace(/[/:]/g, "_") + ".sbom.json";
      a.click();
      URL.revokeObjectURL(a.href);
    },
  },
  template: `
  <section>
    <a class="back" href="#library">← Library</a>
    <empty-state v-if="error" :title="error"></empty-state>
    <skeleton-block v-else-if="!d" h="120px" :n="3"></skeleton-block>
    <template v-else>
      <div class="detail-head">
        <ref-avatar :reference="d.name" size="52"></ref-avatar>
        <div>
          <h1 class="title" style="margin:0"><ref-name :reference="d.name"></ref-name></h1>
          <div class="sub" style="margin:2px 0 0">{{ d.os }}/{{ d.arch }} · added {{ d.created }}</div>
        </div>
      </div>

      <div class="stat-grid">
        <stat-card :value="d.files" label="files"></stat-card>
        <stat-card :value="d.logical_size" fmt="bytes" label="total size"></stat-card>
        <stat-card :text="d.missing_blobs === 0 ? 'yes' : d.missing_blobs + ' missing'" label="offline-ready"></stat-card>
        <stat-card :text="d.pinned ? 'pinned' : 'no'" label="cleanup protection"></stat-card>
      </div>

      <div class="card">
        <span class="label">Run it</span>
        <div class="row" style="margin:8px 0 14px">
          <button class="btn sm primary" @click="run">▶ Start from the browser</button>
          <span style="color:var(--muted);font-size:13px">runs the app's default command; watch it under \u201cRunning\u201d</span>
        </div>
        <div class="copybox"><span>{{ d.run_command }}</span><button class="btn sm ghost" @click="copy(d.run_command)">copy</button></div>
        <div style="height:8px"></div>
        <div class="copybox"><span>{{ d.shell_command }}</span><button class="btn sm ghost" @click="copy(d.shell_command)">copy</button></div>
        <div style="color:var(--muted);font-size:12.5px;margin-top:10px">Interactive shells stay in the terminal; long-running services can be started right here.</div>
      </div>

      <div class="card">
        <span class="label">Technical details</span>
        <div style="margin:10px 0 14px">
          <div class="copybox"><span>{{ d.name }}</span><button class="btn sm ghost" @click="copy(d.name)">copy</button></div>
          <div style="height:8px"></div>
          <div class="copybox"><span>{{ d.id }}</span><button class="btn sm ghost" @click="copy(d.id)">copy</button></div>
          <div style="color:var(--muted);font-size:12.5px;margin-top:8px">The full source name and the manifest id — the content-addressed identity of this exact set of files.</div>
        </div>
        <div class="meta-grid">
          <div class="m"><div class="k">platform</div><div class="v">{{ d.os }}/{{ d.arch }}</div></div>
          <div class="m"><div class="k">created</div><div class="v">{{ d.created }}</div></div>
          <div class="m" v-if="d.origin"><div class="k">origin</div><div class="v">{{ d.origin }}</div></div>
          <div class="m" v-for="r in cfgRows" :key="r[0]"><div class="k">{{ r[0] }}</div><div class="v">{{ r[1] }}</div></div>
        </div>
        <template v-if="d.config.env.length">
          <span class="label" style="margin-top:18px">Environment variables</span>
          <div class="mono" style="margin-top:4px"><div v-for="e in d.config.env" :key="e">{{ e }}</div></div>
        </template>
      </div>

      <div class="card">
        <span class="label">Packages (SBOM)</span>
        <div style="color:var(--muted);font-size:13px;margin-top:6px">{{ sbomStatus }}</div>
        <table v-if="sbom && sbom.packages.length" style="margin-top:10px">
          <thead><tr><th>Package</th><th>Version</th><th>Arch</th></tr></thead>
          <tbody>
            <tr v-for="p in sbom.packages" :key="p.name + p.version">
              <td>{{ p.name }}</td>
              <td class="mono">{{ p.version }}</td>
              <td class="mono" style="color:var(--faint)">{{ p.architecture || '' }}</td>
            </tr>
          </tbody>
        </table>
        <div class="row" style="margin-top:12px">
          <button class="btn sm ghost" :disabled="!sbom" @click="downloadSbom">Download full SBOM (JSON)</button>
        </div>
      </div>

      <div class="card">
        <span class="label">Files <span class="pill gray">{{ filter ? filtered.length + ' match' : d.entries.length + ' entries' }}</span></span>
        <input type="text" v-model="filterInput" placeholder="filter paths… e.g. /etc or busybox"
               style="width:100%;margin:10px 0" spellcheck="false">
        <div class="filelist">
          <div v-for="e in shown" :key="e.path" class="f" :class="{ dir: e.kind === 'dir', link: e.kind === 'symlink' }">
            <span class="k">{{ fileKind(e) }}</span>
            <span class="p" :title="e.blake3 || ''">{{ fileLabel(e) }}</span>
            <span class="s">{{ e.kind === 'file' ? fmtBytes(e.size) : '' }}</span>
          </div>
          <div v-if="filtered.length > 800" class="f">
            <span class="p" style="color:var(--faint)">… {{ filtered.length - 800 }} more (refine the filter)</span>
          </div>
        </div>
      </div>
    </template>
  </section>`,
};
