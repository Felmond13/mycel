// Stacks list: create a stack by name, start/stop whole stacks.

import { api } from "../api.js";
import { toast, confirmDialog } from "../ui.js";

export default {
  name: "StacksPage",
  data() { return { stacks: null, newName: "", timer: null, busy: {} }; },
  mounted() {
    this.load();
    this.timer = setInterval(() => { if (!document.hidden) this.load(); }, 3000);
  },
  unmounted() { clearInterval(this.timer); },
  methods: {
    async load() {
      try { this.stacks = (await api.get("/api/stacks")).stacks; }
      catch (_) { if (this.stacks === null) this.stacks = []; }
    },
    dotKind(s) {
      if (s.running === s.services && s.services > 0) return "green";
      return s.running > 0 ? "amber" : "gray";
    },
    create() {
      const name = this.newName.trim().toLowerCase();
      if (!/^[a-z0-9][a-z0-9_-]{0,31}$/.test(name)) {
        toast("Stack names use lowercase letters, digits, - and _ (max 32 chars)", true);
        return;
      }
      location.hash = "stackedit/" + encodeURIComponent(name);
    },
    async up(s) {
      this.busy[s.name] = true;
      try {
        const r = await api.post("/api/stacks/" + encodeURIComponent(s.name) + "/up");
        const n = r.started.length;
        toast(n ? "Started " + n + " app" + (n === 1 ? "" : "s") : "Everything was already running");
        this.load();
      } catch (e) { toast(e.message, true); this.load(); }
      delete this.busy[s.name];
    },
    async down(s) {
      const ok = await confirmDialog("Stop every app in \u201c" + s.name + "\u201d?",
        "All " + s.services + " apps in this stack are shut down. Start them again any time.", "Stop all");
      if (!ok) return;
      this.busy[s.name] = true;
      try {
        const r = await api.post("/api/stacks/" + encodeURIComponent(s.name) + "/down");
        toast("Stopped " + r.stopped + " app" + (r.stopped === 1 ? "" : "s"));
        this.load();
      } catch (e) { toast(e.message, true); }
      delete this.busy[s.name];
    },
  },
  template: `
  <section>
    <h1 class="title">Stacks — <b>apps that travel together</b></h1>
    <div class="sub">A stack is a group of apps that start together, in the right order — like a database plus the app that uses it.</div>

    <div class="card">
      <div class="row">
        <input type="text" v-model="newName" @keydown.enter="create"
               placeholder="name for a new stack, e.g. my-blog" style="flex:1;min-width:220px" spellcheck="false">
        <button class="btn primary" @click="create">Create stack</button>
      </div>
    </div>

    <skeleton-block v-if="stacks === null" h="76px" :n="2"></skeleton-block>
    <empty-state v-else-if="!stacks.length" title="No stacks yet">
      Give one a name above and press Create — then add apps to it with a simple form.<br>
      Example: a stack called <b>my-site</b> with Nginx and Redis inside.
    </empty-state>
    <template v-else>
      <div v-for="s in stacks" :key="s.name" class="run-card lift">
        <template v-if="s.error">
          <status-dot kind="red"></status-dot>
          <div class="info">
            <div class="nm">{{ s.name }} <span class="pill red">broken</span></div>
            <div class="im">{{ s.error }}</div>
          </div>
        </template>
        <template v-else>
          <status-dot :kind="dotKind(s)" :live="s.running > 0 && s.running === s.services"></status-dot>
          <div class="info">
            <div class="nm"><a :href="'#stack/' + encodeURIComponent(s.name)">{{ s.name }}</a>
              <span v-if="s.network === 'pod'" class="pill teal" title="the apps of this stack share one private network">private network</span>
            </div>
            <div class="im">{{ s.services }} app{{ s.services === 1 ? '' : 's' }} · {{ s.running }} running</div>
          </div>
          <div class="row">
            <button class="btn sm primary" :disabled="s.running === s.services || busy[s.name]" @click="up(s)">
              <span v-if="busy[s.name]" class="spinner"></span>Start all
            </button>
            <button class="btn sm ghost" :disabled="s.running === 0 || busy[s.name]" @click="down(s)">Stop all</button>
            <a :href="'#stack/' + encodeURIComponent(s.name)"><button class="btn sm ghost">Open</button></a>
          </div>
        </template>
      </div>
    </template>
  </section>`,
};
