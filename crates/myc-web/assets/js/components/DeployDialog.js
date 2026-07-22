// "Deploy" dialog for one library app: ship it to any Linux server over
// SSH (the web face of `myc deploy`). Only the files the server is missing
// travel, so the second deploy of an app is near-instant — the final stats
// make that visible. The deploy runs as a server-side job; this dialog
// polls it and shows honest progress ("sending file 3 of 12…").
//
// The dashboard's ssh runs non-interactively with the server user's keys
// (the WSL user's ~/.ssh when Mycel runs inside WSL) — password prompts
// are impossible, and the API explains how to set up a key when a server
// asks for one.

import { api } from "../api.js";
import { fmtBytes, shortRef } from "../format.js";

const TARGET_KEY = "mycel.deployTarget";

export default {
  name: "DeployDialog",
  props: { env: { type: Object, required: true } },
  emits: ["close"],
  data() {
    return {
      target: localStorage.getItem(TARGET_KEY) || "",
      run: true,
      exposed: [],      // the app's own ports, from the detail endpoint
      port: "",         // server port for the first exposed port
      deploying: false,
      job: null,        // live job JSON while polling
      done: null,       // final result (job.result)
      fail: "",
    };
  },
  async created() {
    // The library row doesn't carry the runtime config; fetch it for the
    // port hint. The dialog works fine without it.
    try {
      const d = await api.get("/api/envs/" + encodeURIComponent(this.env.id));
      this.exposed = (d.config && d.config.exposed_ports) || [];
      if (this.exposed.length) this.port = String(this.exposed[0]);
    } catch (_) { /* no port section, that's all */ }
  },
  computed: {
    title() { return shortRef(this.env.name); },
    progressText() {
      return (this.job && this.job.message) || "Connecting…";
    },
    doneText() {
      const r = this.done;
      if (!r) return "";
      const n = (x) => Number(x || 0).toLocaleString("en-US");
      const secs = (r.elapsed_ms / 1000).toFixed(1);
      if (!r.blobs_sent) {
        return "Nothing to send — the server already had every one of the "
          + n(r.blobs_total) + " files. Deployed in " + secs + "s.";
      }
      return n(r.blobs_sent) + " file" + (r.blobs_sent === 1 ? "" : "s")
        + " sent (" + fmtBytes(r.bytes_sent) + "), "
        + n(r.blobs_already_present) + " already there — deployed in " + secs + "s.";
    },
    startedText() {
      const r = this.done;
      if (!r || !r.started) return "";
      let t = "The app is running on the server (pid " + (r.pid || "?") + ")";
      if (r.ports && r.ports.length) {
        t += " — port" + (r.ports.length === 1 ? " " : "s ") + r.ports.join(", ");
      }
      return t + ".";
    },
  },
  methods: {
    async deploy() {
      const target = this.target.trim();
      if (!target) {
        this.fail = "Type the server address first — user@host, e.g. deploy@myserver.com";
        return;
      }
      localStorage.setItem(TARGET_KEY, target);
      this.deploying = true;
      this.job = null;
      this.done = null;
      this.fail = "";
      const body = { ref: this.env.name, target, run: this.run };
      const chosen = parseInt(this.port, 10);
      if (this.run && this.exposed.length && chosen && chosen !== this.exposed[0]) {
        body.ports = [{ host: chosen, container: this.exposed[0] }];
      }
      try {
        const res = await api.post("/api/deploy", body);
        await this.poll(res.job);
      } catch (e) {
        this.fail = e.message;
      }
      this.deploying = false;
    },
    async poll(id) {
      for (;;) {
        const j = await api.get("/api/deploy/" + id);
        this.job = j;
        if (j.status === "done") { this.done = j.result; return; }
        if (j.status === "failed") { this.fail = j.message; return; }
        await new Promise((resolve) => setTimeout(resolve, 700));
      }
    },
  },
  template: `
  <div class="overlay" @click.self="$emit('close')">
    <div class="dialog" style="width:min(540px,92vw)">
      <h3>Deploy {{ title }}</h3>
      <p style="margin-bottom:16px">
        Put this app on your own server. Only the files the server doesn't
        already have are sent — updating later costs almost nothing.
      </p>

      <span class="label">Server</span>
      <p style="margin:4px 0 8px;color:var(--muted);font-size:13px">
        Any Linux server you can reach with ssh, as <span class="mono">user@host</span>.
        Mycel is installed on it automatically if it's missing.
      </p>
      <input type="text" v-model="target" @keydown.enter="deploy" :disabled="deploying"
             placeholder="deploy@myserver.com" style="width:100%;margin-bottom:12px" spellcheck="false">

      <label class="row" style="gap:8px;cursor:pointer;margin-bottom:4px">
        <input type="checkbox" v-model="run" :disabled="deploying">
        <span>Start the app after deploy</span>
      </label>
      <div v-if="run && exposed.length" class="row" style="gap:8px;margin:4px 0 8px 24px;align-items:center">
        <span style="color:var(--muted);font-size:13px">Reachable on server port</span>
        <input type="text" v-model="port" :disabled="deploying" inputmode="numeric"
               style="width:76px" spellcheck="false">
        <span v-if="String(exposed[0]) !== port" style="color:var(--muted);font-size:12px">
          (the app's own port {{ exposed[0] }} gets forwarded)
        </span>
      </div>

      <div class="row" style="margin-top:10px">
        <button class="btn primary" :disabled="deploying" @click="deploy">
          <span v-if="deploying" class="spinner"></span>{{ deploying ? 'Deploying…' : 'Deploy' }}
        </button>
        <span v-if="deploying" style="color:var(--muted);font-size:13px">{{ progressText }}</span>
      </div>

      <div v-if="done" style="color:var(--green);font-size:13px;margin-top:10px">
        ✓ {{ doneText }}<template v-if="startedText"><br>✓ {{ startedText }}</template>
      </div>
      <div v-if="fail" style="color:var(--red);font-size:13px;margin-top:10px;white-space:pre-wrap">✗ {{ fail }}</div>

      <div class="row" style="justify-content:flex-end;margin-top:18px">
        <button class="btn ghost" @click="$emit('close')">Close</button>
      </div>
    </div>
  </div>`,
};
