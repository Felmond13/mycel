// One stack: its services in dependency order, live states, missing-image
// banner with one-click install, and whole-stack controls.

import { S, installImageAndWait } from "../store.js";
import { api } from "../api.js";
import { toast, confirmDialog, openLogs } from "../ui.js";
import { fmtDur, fmtBytes, shortRef } from "../format.js";
import { fmtCpu } from "../metrics.js";

export default {
  name: "StackView",
  props: { arg: { type: String, required: true } },
  data() {
    return {
      d: null, error: "", installingMissing: false, timer: null,
      upBusy: false, downBusy: false, svcBusy: {},
    };
  },
  computed: {
    missing() { return this.d ? this.d.services.filter(s => !s.installed).map(s => s.image) : []; },
    anyRunning() { return this.d && this.d.services.some(s => s.state && s.state.running); },
  },
  mounted() {
    this.load();
    this.timer = setInterval(() => { if (!document.hidden) this.load(); }, 2500);
  },
  unmounted() { clearInterval(this.timer); },
  methods: {
    async load() {
      try {
        this.d = await api.get("/api/stacks/" + encodeURIComponent(this.arg));
        this.error = "";
      } catch (e) {
        // A stack that exists only in the editor (never saved) has no file yet.
        this.error = e.message;
        clearInterval(this.timer);
      }
    },
    stateText(s) {
      const st = s.state;
      if (!st) return "not started";
      if (st.running) return "running " + fmtDur(S.now - st.started_at);
      if (st.stopped_by_user) return "stopped by you";
      if (st.failure_hint) return "stopped with an error — " + st.failure_hint;
      if (st.exit_code === 0) return "finished";
      if (st.exit_code === null) return "stopped";
      return "stopped with an error (code " + st.exit_code + ")";
    },
    dotKind(s) {
      const st = s.state;
      if (!st) return "gray";
      if (st.running) return "green";
      return (st.stopped_by_user || st.exit_code === 0 || st.exit_code === null) ? "gray" : "red";
    },
    async up() {
      this.upBusy = true;
      try {
        const r = await api.post("/api/stacks/" + encodeURIComponent(this.arg) + "/up");
        const n = r.started.length;
        toast(n ? "Started " + n + " app" + (n === 1 ? "" : "s") : "Everything was already running");
        this.load();
      } catch (e) { toast(e.message, true); this.load(); }
      this.upBusy = false;
    },
    async down() {
      const ok = await confirmDialog("Stop every app in \u201c" + this.arg + "\u201d?",
        "All apps in this stack are shut down. Start them again any time.", "Stop all");
      if (!ok) return;
      this.downBusy = true;
      try {
        const r = await api.post("/api/stacks/" + encodeURIComponent(this.arg) + "/down");
        toast("Stopped " + r.stopped + " app" + (r.stopped === 1 ? "" : "s"));
        this.load();
      } catch (e) { toast(e.message, true); }
      this.downBusy = false;
    },
    async del() {
      const ok = await confirmDialog("Delete stack \u201c" + this.arg + "\u201d?",
        "Running apps in it are stopped first. The stack file is deleted; installed apps stay in the store.", "Delete");
      if (!ok) return;
      try {
        await api.del("/api/stacks/" + encodeURIComponent(this.arg));
        toast("Stack deleted");
        location.hash = "stacks";
      } catch (e) { toast(e.message, true); }
    },
    async restartSvc(s) {
      this.svcBusy[s.name] = true;
      try {
        await api.post("/api/stacks/" + encodeURIComponent(this.arg) + "/services/" + encodeURIComponent(s.name) + "/restart");
        toast(s.name + " started");
        this.load();
      } catch (e) { toast(e.message, true); this.load(); }
      delete this.svcBusy[s.name];
    },
    async installMissing() {
      this.installingMissing = true;
      for (const image of this.missing) {
        try { await installImageAndWait(image); toast(shortRef(image) + " added to your library"); }
        catch (e) { toast(e.message, true); }
      }
      this.installingMissing = false;
      this.load();
    },
    svcLogs(s) { if (s.state) openLogs(s.state.id, this.arg + " / " + s.name, s.image); },
    shortRef, fmtBytes, fmtCpu,
  },
  template: `
  <section>
    <a class="back" href="#stacks">← all stacks</a>

    <empty-state v-if="error" :title="error">
      <a :href="'#stackedit/' + encodeURIComponent(arg)"><button class="btn primary">Create it in the editor →</button></a>
    </empty-state>
    <skeleton-block v-else-if="!d" h="76px" :n="3"></skeleton-block>
    <template v-else>
      <div class="row spread" style="margin-bottom:6px">
        <h1 class="title" style="margin:0"><b>{{ d.name }}</b></h1>
        <div class="row">
          <button class="btn sm primary" :disabled="upBusy" @click="up">
            <span v-if="upBusy" class="spinner"></span>{{ upBusy ? 'Starting…' : '▶ Start all' }}
          </button>
          <button class="btn sm ghost" :disabled="!anyRunning || downBusy" @click="down">
            <span v-if="downBusy" class="spinner"></span>{{ downBusy ? 'Stopping…' : '■ Stop all' }}
          </button>
          <a :href="'#stackedit/' + encodeURIComponent(arg)"><button class="btn sm ghost">Edit</button></a>
          <button class="btn sm danger" @click="del">Delete</button>
        </div>
      </div>
      <div class="sub">Apps start in dependency order. They share your computer's network, so they find each other on localhost.</div>

      <div v-if="missing.length" class="card" style="border-color:rgba(251,191,36,.4)">
        <b style="color:var(--amber)">Some apps in this stack aren't in your library yet</b>
        <div style="color:var(--muted);font-size:13px;margin:6px 0 12px">{{ missing.map(shortRef).join(', ') }}</div>
        <button class="btn sm primary" :disabled="installingMissing" @click="installMissing">
          <span v-if="installingMissing" class="spinner"></span>{{ installingMissing ? 'Fetching…' : 'Get them now' }}
        </button>
      </div>

      <div v-for="s in d.services" :key="s.name" class="run-card lift">
        <status-dot :kind="dotKind(s)" :live="!!(s.state && s.state.running)"></status-dot>
        <ref-avatar :reference="s.image" size="36"></ref-avatar>
        <div class="info">
          <div class="nm">
            {{ s.name }}
            <span v-if="s.depends_on.length" class="pill gray" title="starts after">after {{ s.depends_on.join(', ') }}</span>
          </div>
          <div class="im"><ref-name :reference="s.image" :bold="false"></ref-name><template v-if="s.command.length"> — {{ s.command.join(' ') }}</template></div>
        </div>
        <div class="st">
          {{ stateText(s) }}
          <div v-if="s.state && s.state.running && typeof s.state.cpu_percent === 'number'"
               class="mono" style="margin-top:3px;font-size:11.5px;color:var(--teal)">
            <a :href="'#running/' + encodeURIComponent(s.state.id)" style="color:inherit"
               title="inspect this container on the Running page">
              CPU {{ fmtCpu(s.state.cpu_percent) }} · RAM {{ fmtBytes(s.state.memory_bytes) }}
            </a>
          </div>
        </div>
        <div class="row">
          <button v-if="s.state" class="btn sm ghost" @click="svcLogs(s)">Logs</button>
          <button class="btn sm ghost" :disabled="svcBusy[s.name]" @click="restartSvc(s)">
            <span v-if="svcBusy[s.name]" class="spinner"></span>{{ s.state && s.state.running ? 'Restart' : 'Start' }}
          </button>
        </div>
      </div>
    </template>
  </section>`,
};
