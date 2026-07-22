// "Customize & start" modal for a catalog app: the one-click defaults
// (name, port, env) prefilled and editable for power users. Emits
// `start` with {name, port, env, keep_data, data_folder, isolate, ports}
// overrides, or `close`.
//
// The Data section only appears for apps that declare data directories
// (redis, postgres, …): a "keep my data" toggle (ON by default — the
// server behaves the same when the dialog is skipped) and, for advanced
// users, a folder of the computer to use instead of a managed volume.
//
// The Network section offers the optional "own private network" mode:
// each port the app declares gets an editable "available on localhost:…"
// field. When pasta is missing on the machine the toggle is disabled
// with the install hint (the server would refuse anyway).

import { api } from "../api.js";

export default {
  name: "StartDialog",
  props: { entry: { type: Object, required: true } },
  emits: ["start", "close"],
  data() {
    return {
      name: this.entry.id,
      port: this.entry.port ? String(this.entry.port) : "",
      env: (this.entry.env || []).map(kv => {
        const eq = kv.indexOf("=");
        return { k: eq === -1 ? kv : kv.slice(0, eq), v: eq === -1 ? "" : kv.slice(eq + 1) };
      }),
      volumes: [],        // data directories the image declares
      keepData: true,
      useFolder: false,
      dataFolder: "",
      isolate: false,     // own private network (server: isolate_network)
      isoAvailable: null, // null while loading /api/capabilities
      netPorts: [],       // [{container, host}] — host is an editable string
    };
  },
  computed: {
    command() { return (this.entry.command || []).join(" "); },
    portNum() {
      const n = parseInt(this.port, 10);
      return Number.isInteger(n) && n > 0 && n < 65536 ? n : null;
    },
    portBad() { return this.port.trim() !== "" && this.portNum === null; },
    folderBad() {
      return this.keepData && this.useFolder && this.dataFolder.trim() !== ""
        && !this.dataFolder.trim().startsWith("/");
    },
    netPortsBad() {
      if (!this.isolate) return false;
      return this.netPorts.some(r => {
        const n = parseInt(r.host, 10);
        return !(Number.isInteger(n) && n > 0 && n < 65536);
      });
    },
    cantSubmit() { return this.portBad || this.folderBad || this.netPortsBad; },
  },
  async mounted() {
    // Does this app declare data directories or ports? (404 = not
    // installed yet.)
    try {
      const d = await api.get("/api/envs/" + encodeURIComponent(this.entry.image));
      this.volumes = (d.config && d.config.volumes) || [];
      const declared = (d.config && d.config.exposed_ports) || [];
      const ports = declared.length ? declared
        : (this.entry.port ? [this.entry.port] : []);
      this.netPorts = ports.map(p => ({ container: p, host: String(p) }));
    } catch (_) { this.volumes = []; this.netPorts = []; }
    try {
      const caps = await api.get("/api/capabilities");
      this.isoAvailable = !!caps.network_isolation;
    } catch (_) { this.isoAvailable = false; }
  },
  methods: {
    addEnv() { this.env.push({ k: "", v: "" }); },
    dropEnv(i) { this.env.splice(i, 1); },
    submit() {
      if (this.cantSubmit) return;
      const env = this.env
        .filter(r => r.k.trim())
        .map(r => r.k.trim() + "=" + r.v);
      const folder = this.keepData && this.useFolder ? this.dataFolder.trim() : "";
      this.$emit("start", {
        name: this.name.trim() || this.entry.id,
        port: this.portNum,
        env,
        keep_data: this.keepData,
        data_folder: folder || null,
        isolate: this.isolate,
        ports: this.isolate
          ? this.netPorts.map(r => ({ container: r.container, host: parseInt(r.host, 10) }))
          : [],
      });
    },
  },
  template: `
  <div class="overlay" @click.self="$emit('close')">
    <div class="dialog" style="width:min(520px,92vw)">
      <h3>Start {{ entry.title }} — your way</h3>
      <p style="margin-bottom:16px">These are the defaults the one-click Start uses. Change what you need.</p>

      <span class="label">Name</span>
      <input type="text" v-model="name" style="width:100%;margin-bottom:14px" spellcheck="false">

      <span class="label">Port (for the "Open" link — the app itself decides what it listens on)</span>
      <input type="text" v-model="port" placeholder="none" style="width:140px;margin-bottom:4px" spellcheck="false">
      <div v-if="portBad" style="color:var(--red);font-size:12px;margin-bottom:10px">must be a number between 1 and 65535</div>
      <div v-else style="height:10px"></div>

      <template v-if="volumes.length">
        <span class="label">Data</span>
        <label class="row" style="gap:8px;cursor:pointer;margin-bottom:4px">
          <input type="checkbox" v-model="keepData">
          <span>Keep this app's data between restarts</span>
        </label>
        <div v-if="keepData" style="color:var(--muted);font-size:12px;margin:0 0 8px 24px">
          Saved automatically — stop or restart {{ entry.title }} anytime, it finds its data again.
        </div>
        <div v-else style="color:var(--muted);font-size:12px;margin:0 0 8px 24px">
          Everything this app writes will be gone when it stops.
        </div>
        <label v-if="keepData" class="row" style="gap:8px;cursor:pointer;margin:0 0 4px 24px;font-size:13px">
          <input type="checkbox" v-model="useFolder">
          <span>Use a folder of this computer instead</span>
        </label>
        <div v-if="keepData && useFolder" style="margin:0 0 4px 24px">
          <input type="text" v-model="dataFolder" placeholder="/home/you/my-app-data"
                 style="width:100%" spellcheck="false" class="mono">
          <div v-if="folderBad" style="color:var(--red);font-size:12px;margin-top:3px">must be an absolute path (starting with /)</div>
        </div>
        <div style="height:10px"></div>
      </template>

      <span class="label">Network</span>
      <label class="row" style="gap:8px;cursor:pointer;margin-bottom:4px"
             :style="isoAvailable === false ? 'opacity:.55' : ''">
        <input type="checkbox" v-model="isolate" :disabled="isoAvailable === false">
        <span>Give this app its own private network</span>
      </label>
      <div v-if="isoAvailable === false" style="color:var(--muted);font-size:12px;margin:0 0 8px 24px">
        Not available on this computer yet — it needs the 'passt' package
        (in a terminal: <span class="mono">sudo apt install passt</span>).
      </div>
      <div v-else-if="!isolate" style="color:var(--muted);font-size:12px;margin:0 0 8px 24px">
        Usually not needed — if a port is busy, Mycel switches this on by
        itself and picks a free port nearby.
      </div>
      <template v-else>
        <div style="color:var(--muted);font-size:12px;margin:0 0 8px 24px">
          The app runs in its own bubble: it can reach the internet, and
          only the doors below are open on this computer.
        </div>
        <div v-for="r in netPorts" :key="r.container" class="row"
             style="gap:8px;margin:0 0 6px 24px;font-size:13px">
          <span>Port {{ r.container }} — available on localhost:</span>
          <input type="text" v-model="r.host" style="width:80px" spellcheck="false" class="mono">
        </div>
        <div v-if="!netPorts.length" style="color:var(--muted);font-size:12px;margin:0 0 8px 24px">
          This app doesn't declare any ports — nothing to open.
        </div>
        <div v-if="netPortsBad" style="color:var(--red);font-size:12px;margin:0 0 8px 24px">
          each port must be a number between 1 and 65535
        </div>
      </template>
      <div style="height:10px"></div>

      <span class="label">Environment variables</span>
      <div v-for="(r, i) in env" :key="i" class="kvrow">
        <input type="text" v-model="r.k" placeholder="KEY" spellcheck="false">
        <input type="text" v-model="r.v" placeholder="value" spellcheck="false">
        <button class="iconbtn danger" title="remove" @click="dropEnv(i)">✕</button>
      </div>
      <button class="btn sm ghost" style="margin-bottom:14px" @click="addEnv">+ add variable</button>

      <template v-if="command">
        <span class="label">Command</span>
        <div class="mono" style="color:var(--muted);font-size:12px;margin-bottom:14px">{{ command }}</div>
      </template>

      <div class="row" style="justify-content:flex-end">
        <button class="btn ghost" @click="$emit('close')">Cancel</button>
        <button class="btn primary" :disabled="cantSubmit" @click="submit">Start</button>
      </div>
    </div>
  </div>`,
};
