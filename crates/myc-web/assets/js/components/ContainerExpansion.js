// Accordion panel under a container row: what is INSIDE the container
// (live process tree with per-process CPU/RSS), the launch details it was
// started with (command, env with secrets masked, port…), and CPU/RAM
// sparklines. Polls /api/containers/{id}/processes only while mounted.

import { api } from "../api.js";
import { fmtBytes } from "../format.js";
import { historyFor, fmtCpu } from "../metrics.js";

const PROC_CAP = 50;
const SECRET = /pass|secret|token|key|credential/i;

export default {
  name: "ContainerExpansion",
  props: { c: { type: Object, required: true } },
  emits: ["logs", "restart", "stop", "remove"],
  data() {
    return { procs: null, procError: "", revealEnv: false, timer: null,
             mounts: null, copiedPath: "" };
  },
  computed: {
    hist() { return historyFor(this.c.id); },
    shownProcs() { return (this.procs || []).slice(0, PROC_CAP); },
    moreProcs() { return Math.max(0, (this.procs || []).length - PROC_CAP); },
    envRows() {
      return (this.c.env || []).map(kv => {
        const eq = kv.indexOf("=");
        const k = eq === -1 ? kv : kv.slice(0, eq);
        const v = eq === -1 ? "" : kv.slice(eq + 1);
        return { k, v, secret: SECRET.test(k) };
      });
    },
    hasSecrets() { return this.envRows.some(r => r.secret); },
    startedAt() { return new Date(this.c.started_at * 1000).toLocaleString(); },
    exitText() {
      const c = this.c;
      if (c.exit_code === null) return "unknown (ended while the dashboard was off)";
      // 143/137 after a Stop click are the normal shutdown signals, not errors.
      if (c.stopped_by_user) return c.exit_code + " (stopped by you — this is normal)";
      return c.exit_code;
    },
  },
  mounted() {
    this.poll();
    this.loadMounts();
    this.timer = setInterval(() => { if (!document.hidden) this.poll(); }, 2000);
  },
  unmounted() { clearInterval(this.timer); },
  methods: {
    fmtBytes, fmtCpu,
    envValue(r) { return r.secret && !this.revealEnv ? "••••••••" : r.v; },
    async loadMounts() {
      try {
        const r = await api.get("/api/containers/" + encodeURIComponent(this.c.id) + "/data");
        this.mounts = r.data;
      } catch (_) { this.mounts = []; }
    },
    mountTitle(m) {
      return m.kind === "volume"
        ? "Saved by Mycel — survives stops and restarts"
        : "A folder of this computer";
    },
    async copyPath(m) {
      // Windows users get the Explorer-friendly spelling when it exists.
      const path = m.windows_path || m.host_path;
      try { await navigator.clipboard.writeText(path); } catch (_) { /* http fallback below */ }
      this.copiedPath = m.container_path;
      setTimeout(() => { this.copiedPath = ""; }, 1600);
    },
    async poll() {
      if (!this.c.running) { this.procs = []; return; }
      try {
        const r = await api.get("/api/containers/" + encodeURIComponent(this.c.id) + "/processes");
        this.procs = r.processes;
        this.procError = "";
      } catch (e) { this.procError = e.message; }
    },
  },
  template: `
  <div class="expand-panel">
    <div class="expand-cols">
      <div class="expand-col">
        <span class="label">Inside this container</span>
        <div v-if="!c.running" class="expand-note">Not running — nothing inside to show.</div>
        <div v-else-if="procError" class="expand-note" style="color:var(--red)">{{ procError }}</div>
        <div v-else-if="procs === null" class="expand-note">Reading processes…</div>
        <div v-else-if="!procs.length" class="expand-note">No processes visible (it may be shutting down).</div>
        <table v-else class="proc-table">
          <thead><tr><th>Process</th><th class="num">PID</th><th class="num">CPU</th><th class="num">Memory</th></tr></thead>
          <tbody>
            <tr v-for="p in shownProcs" :key="p.pid">
              <td class="mono">{{ p.name }}</td>
              <td class="num mono">{{ p.pid }}</td>
              <td class="num mono">{{ fmtCpu(p.cpu_percent) }}</td>
              <td class="num mono">{{ fmtBytes(p.memory_bytes) }}</td>
            </tr>
          </tbody>
        </table>
        <div v-if="moreProcs" class="expand-note">+ {{ moreProcs }} more process{{ moreProcs === 1 ? '' : 'es' }}</div>

        <template v-if="c.running && hist.cpu.length > 1">
          <div class="spark-row">
            <div class="spark-box">
              <div class="k">CPU <span class="mono">{{ fmtCpu(c.cpu_percent) }}</span></div>
              <spark-line :values="hist.cpu" :max="100" color="var(--teal)"></spark-line>
            </div>
            <div class="spark-box">
              <div class="k">RAM <span class="mono">{{ fmtBytes(c.memory_bytes) }}</span></div>
              <spark-line :values="hist.mem" color="var(--violet)"></spark-line>
            </div>
          </div>
        </template>
      </div>

      <div class="expand-col">
        <span class="label">How it was launched</span>
        <div class="meta-grid" style="margin-top:8px">
          <div class="m"><div class="k">app</div><div class="v">{{ c.reference }}</div></div>
          <div class="m"><div class="k">command</div><div class="v">{{ c.command && c.command.length ? c.command.join(' ') : "(app's default)" }}</div></div>
          <div class="m" v-if="c.workdir"><div class="k">workdir</div><div class="v">{{ c.workdir }}</div></div>
          <div class="m" v-if="c.network === 'isolated'">
            <div class="k">network</div>
            <div class="v">its own private network</div>
          </div>
          <div class="m" v-if="c.ports && c.ports.length">
            <div class="k">reachable at</div>
            <div class="v">
              <div v-for="m in c.ports" :key="m.host">
                localhost:{{ m.host }}<span v-if="m.host !== m.container"
                  style="color:var(--muted)"> — the app thinks it's {{ m.container }}</span>
              </div>
            </div>
          </div>
          <div class="m" v-else><div class="k">port</div><div class="v">{{ c.port ? 'localhost:' + c.port : '(none declared)' }}</div></div>
          <div class="m"><div class="k">started</div><div class="v">{{ startedAt }}</div></div>
          <div class="m" v-if="!c.running"><div class="k">exit code</div><div class="v">{{ exitText }}</div></div>
          <div class="m" v-if="!c.running && c.failure_hint"><div class="k">why it stopped</div><div class="v" style="color:var(--red)">{{ c.failure_hint }}</div></div>
          <div class="m" v-if="c.running && c.disk_bytes != null"><div class="k">disk (rootfs)</div><div class="v">{{ fmtBytes(c.disk_bytes) }}</div></div>
          <div class="m"><div class="k">container id</div><div class="v">{{ c.id }}</div></div>
        </div>

        <template v-if="mounts && mounts.length">
          <span class="label" style="margin-top:16px">Data</span>
          <div v-for="m in mounts" :key="m.container_path"
               style="padding:7px 0;border-bottom:1px solid var(--line, rgba(255,255,255,.06))">
            <div class="row spread" style="gap:8px">
              <span :title="mountTitle(m)">
                <span class="pill" :class="m.kind === 'volume' ? 'accent' : 'gray'">
                  {{ m.kind === 'volume' ? 'saved' : 'folder' }}</span>
                <span class="mono" style="font-size:12px;margin-left:6px">{{ m.container_path }}</span>
                <span v-if="m.read_only" style="color:var(--muted);font-size:11px"> (read-only)</span>
              </span>
              <span class="mono" style="color:var(--muted);font-size:12px">{{ fmtBytes(m.size_bytes) }}</span>
            </div>
            <div class="row" style="gap:6px;margin-top:3px">
              <span class="mono" style="color:var(--faint);font-size:11px;word-break:break-all;flex:1"
                    :title="m.windows_path || ''">{{ m.host_path }}</span>
              <button class="btn sm ghost" style="flex-shrink:0"
                      :title="m.windows_path ? 'Copies the Windows path — paste it into Explorer' : 'Copies the folder path'"
                      @click="copyPath(m)">
                {{ copiedPath === m.container_path ? 'Copied ✓' : 'Open folder' }}
              </button>
            </div>
          </div>
          <div style="color:var(--muted);font-size:11.5px;margin-top:5px">
            This is where {{ c.name }} keeps its files — they stay put when the app stops.
          </div>
        </template>

        <template v-if="envRows.length">
          <div class="row spread" style="margin-top:16px">
            <span class="label" style="margin:0">Environment variables</span>
            <button v-if="hasSecrets" class="btn sm ghost" @click="revealEnv = !revealEnv">
              {{ revealEnv ? 'Hide secrets' : 'Reveal secrets' }}
            </button>
          </div>
          <div class="mono env-list">
            <div v-for="r in envRows" :key="r.k">{{ r.k }}=<span :class="{ masked: r.secret && !revealEnv }">{{ envValue(r) }}</span></div>
          </div>
        </template>

        <div class="row" style="margin-top:18px">
          <button class="btn sm ghost" @click="$emit('logs')">Logs</button>
          <template v-if="c.running">
            <button class="btn sm ghost" @click="$emit('restart')">Restart</button>
            <button class="btn sm danger" @click="$emit('stop')">Stop</button>
          </template>
          <template v-else>
            <button class="btn sm primary" @click="$emit('restart')">Start again</button>
            <button class="btn sm danger" @click="$emit('remove')">Remove</button>
          </template>
        </div>
      </div>
    </div>
  </div>`,
};
