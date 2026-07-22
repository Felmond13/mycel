// "Running": every container the UI started, as an elegant expandable
// list — click a row to see what's inside (process tree, launch details,
// sparklines). Header strip shows totals; exited rows can be cleared.

import { S, stopContainer, refreshState } from "../store.js";
import { api } from "../api.js";
import { toast, confirmDialog, openLogs } from "../ui.js";
import { fmtDur, fmtBytes } from "../format.js";
import { totals, fmtCpu } from "../metrics.js";
import { route } from "../router.js";

export default {
  name: "ContainersPage",
  data() { return { expanded: null, busy: {}, clearing: false }; },
  // Deep link: #running/<container-id> opens with that row expanded
  // (used by the per-service metrics link on a stack page).
  mounted() { if (route.arg) this.expanded = route.arg; },
  computed: {
    S() { return S; },
    containers() { return S.containers; },
    exited() { return S.containers.filter(c => !c.running); },
    tot() { return totals(S.containers); },
  },
  methods: {
    fmtBytes, fmtCpu,
    dotKind(c) {
      if (c.running) return "green";
      // A stop the user asked for is a success, whatever the exit code
      // (SIGTERM = 143, SIGKILL escalation = 137). exit_code === null: it
      // ended while the dashboard was off — not necessarily an error.
      if (c.stopped_by_user || c.exit_code === 0 || c.exit_code === null) return "gray";
      return "red";
    },
    when(c) {
      return c.running
        ? "up " + fmtDur(S.now - c.started_at)
        : "stopped " + fmtDur(S.now - (c.finished_at || c.started_at)) + " ago";
    },
    toggle(c) { this.expanded = this.expanded === c.id ? null : c.id; },
    async restart(c) {
      this.busy[c.id] = true;
      try {
        const fresh = await api.post("/api/containers/" + encodeURIComponent(c.id) + "/restart");
        // The restart may have moved the app to a nearby free port.
        toast(fresh.port_note || (c.name + (c.running ? " restarted" : " started again")));
        if (this.expanded === c.id) this.expanded = fresh.id;
        refreshState();
      } catch (e) { toast(e.message, true); }
      delete this.busy[c.id];
    },
    async remove(c) {
      const ok = await confirmDialog("Remove " + c.name + "?",
        "It disappears from this list and its log is deleted. The app itself stays installed.", "Remove");
      if (!ok) return;
      this.busy[c.id] = true;
      try {
        await api.del("/api/containers/" + encodeURIComponent(c.id));
        if (this.expanded === c.id) this.expanded = null;
        refreshState();
      } catch (e) { toast(e.message, true); }
      delete this.busy[c.id];
    },
    async clearExited() {
      const n = this.exited.length;
      const ok = await confirmDialog("Clear " + n + " stopped app" + (n === 1 ? "" : "s") + "?",
        "They disappear from this list and their logs are deleted. The apps stay installed.", "Clear all");
      if (!ok) return;
      this.clearing = true;
      let cleared = 0;
      for (const c of this.exited) {
        try { await api.del("/api/containers/" + encodeURIComponent(c.id)); cleared++; }
        catch (e) { toast(e.message, true); }
      }
      this.clearing = false;
      if (cleared) toast("Cleared " + cleared + " stopped app" + (cleared === 1 ? "" : "s"));
      refreshState();
    },
    stop(c) { stopContainer(c.id, c.name); },
    logs(c) { openLogs(c.id, c.name, c.reference); },
  },
  template: `
  <section>
    <h1 class="title">Running <b>apps</b></h1>
    <div class="sub">Everything you started, live. Click a row to look inside the container.</div>

    <skeleton-block v-if="!S.loaded" h="76px" :n="3"></skeleton-block>
    <empty-state v-else-if="!containers.length" title="Nothing is running yet">
      Head to <a href="#apps">Apps</a>, install something and press Start.<br>
      It will show up here with a green light.
    </empty-state>
    <template v-else>
      <div class="stat-grid" style="grid-template-columns:repeat(auto-fit, minmax(150px, 1fr))">
        <stat-card :value="tot.running" label="running now"></stat-card>
        <stat-card :text="fmtCpu(tot.cpu)" label="total CPU (of one core)"></stat-card>
        <stat-card :text="fmtBytes(tot.mem)" label="total RAM"></stat-card>
      </div>

      <div class="applist glass">
        <template v-for="c in containers" :key="c.id">
          <div class="app-row" :class="{ open: expanded === c.id }" @click="toggle(c)">
            <status-dot :kind="dotKind(c)" :live="c.running"></status-dot>
            <ref-avatar :reference="c.reference" size="30"></ref-avatar>
            <div class="info">
              <div class="nm">
                {{ c.name }}
                <a v-if="c.stack" :href="'#stack/' + encodeURIComponent(c.stack)" @click.stop>
                  <span class="pill accent">{{ c.stack }}</span>
                </a>
              </div>
              <div class="im"><ref-name :reference="c.reference" :bold="false"></ref-name></div>
              <div v-if="!c.running && c.failure_hint" class="im" style="color:var(--red)">
                {{ c.failure_hint }}
              </div>
            </div>
            <div class="st">
              <span v-if="c.running" class="pill green">running</span>
              <span v-else-if="c.stopped_by_user" class="pill gray">stopped by you</span>
              <span v-else-if="c.exit_code === 0" class="pill gray">finished</span>
              <span v-else-if="c.exit_code === null" class="pill gray"
                    title="It ended while the dashboard was not running, so the exit code is unknown">stopped</span>
              <span v-else class="pill red" :title="'exit code ' + c.exit_code">stopped with an error</span>
              <div class="when">{{ when(c) }}</div>
            </div>
            <div class="live-metrics mono" v-if="c.running && typeof c.cpu_percent === 'number'">
              CPU {{ fmtCpu(c.cpu_percent) }} · RAM {{ fmtBytes(c.memory_bytes) }}
            </div>
            <div class="row" @click.stop>
              <a v-if="c.running && c.port" :href="'http://localhost:' + c.port" target="_blank">
                <button class="btn sm success">Open ↗</button>
              </a>
              <button class="btn sm ghost" @click="logs(c)">Logs</button>
              <template v-if="c.running">
                <button class="btn sm ghost" :disabled="busy[c.id]" @click="restart(c)">
                  <span v-if="busy[c.id]" class="spinner"></span>Restart
                </button>
                <button class="btn sm danger" @click="stop(c)">Stop</button>
              </template>
              <template v-else>
                <button class="btn sm ghost" :disabled="busy[c.id]" @click="restart(c)">
                  <span v-if="busy[c.id]" class="spinner"></span>Start again
                </button>
                <button class="btn sm danger" :disabled="busy[c.id]" @click="remove(c)">Remove</button>
              </template>
            </div>
            <span class="chev" :class="{ open: expanded === c.id }">›</span>
          </div>
          <container-expansion v-if="expanded === c.id" :c="c"
            @logs="logs(c)" @restart="restart(c)" @stop="stop(c)" @remove="remove(c)">
          </container-expansion>
        </template>
      </div>

      <div class="row spread" style="margin-top:14px">
        <div class="sub" style="margin:0">Stopped apps keep their logs until you remove them.</div>
        <button v-if="exited.length" class="btn sm ghost" :disabled="clearing" @click="clearExited">
          <span v-if="clearing" class="spinner"></span>Clear {{ exited.length }} stopped
        </button>
      </div>
    </template>
  </section>`,
};
