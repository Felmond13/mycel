// Apps catalog: the 3-verb Install / Start / Stop flow, plus the first-run
// hero when the store is completely empty.

import { S, isInstalled, runningContainerFor, installImageAndWait, stopContainer, refreshState } from "../store.js";
import { api } from "../api.js";
import { toast, openLogs } from "../ui.js";
import { fmtDur } from "../format.js";
import { CATALOG } from "../catalog.js";

export default {
  name: "AppsPage",
  data() { return { catalog: CATALOG, customizing: null }; },
  computed: {
    S() { return S; },
    firstRun() { return S.loaded && !S.apiDown && S.envs.length === 0 && S.containers.length === 0; },
  },
  methods: {
    /** "port 8080 · POSTGRES_PASSWORD=mycel" — what Start will use, no magic. */
    presets(entry) {
      const bits = [];
      if (entry.port) bits.push("port " + entry.port);
      for (const kv of entry.env || []) bits.push(kv);
      return bits.join(" · ");
    },
    cardState(entry) {
      const ui = S.appUi[entry.id] || {};
      const running = runningContainerFor(entry.image);
      if (running) return { kind: "running", c: running };
      if (ui.installing) return { kind: "installing", progress: ui.progress };
      if (ui.starting) return { kind: "starting" };
      if (isInstalled(entry.image)) return { kind: "installed" };
      return { kind: "none" };
    },
    upFor(c) { return fmtDur(S.now - c.started_at); },
    async install(entry) {
      S.appUi[entry.id] = { installing: true, progress: "" };
      try {
        await installImageAndWait(entry.image, j => {
          if (S.appUi[entry.id]) {
            S.appUi[entry.id].progress = j.layers_total
              ? Math.round(100 * j.layers_done / j.layers_total) + "%" : "";
          }
        });
        delete S.appUi[entry.id];
        toast(entry.title + " installed — press Start when ready");
        refreshState();
      } catch (e) {
        delete S.appUi[entry.id];
        toast(e.message, true);
      }
    },
    /** One-click start with catalog defaults; `custom` overrides them. */
    async start(entry, custom) {
      this.customizing = null;
      S.appUi[entry.id] = { starting: true };
      const port = custom ? custom.port : entry.port || null;
      try {
        const c = await api.post("/api/containers", {
          reference: entry.image,
          name: custom ? custom.name : entry.id,
          command: entry.command || [],
          env: custom ? custom.env : entry.env || [],
          map_user: entry.map_user || null,
          port,
          // Data directories persist by default (server default too);
          // the Customize dialog can opt out or pick a folder.
          keep_data: custom ? custom.keep_data : true,
          data_folder: custom ? custom.data_folder : null,
          // Optional private network from the Customize dialog. When a
          // port is busy the server turns this on by itself and answers
          // with port_note + the real ports.
          isolate_network: custom ? !!custom.isolate : false,
          ports: custom ? custom.ports || [] : [],
        });
        delete S.appUi[entry.id];
        refreshState();
        if (c.port_note) {
          toast(c.port_note);
        } else {
          const at = c.port || port;
          toast(entry.title + " is starting" + (at ? " — try http://localhost:" + at + " in a few seconds" : ""));
        }
      } catch (e) {
        delete S.appUi[entry.id];
        toast(e.message, true);
      }
    },
    stop(c, title) { stopContainer(c.id, title); },
    logs(c, title) { openLogs(c.id, title, c.reference); },
  },
  template: `
  <section>
    <h1 class="title">Run an app <b>in one click</b></h1>
    <div class="sub">Pick an app, press Install, then Start. That's it — no terminal needed.</div>

    <div v-if="firstRun" class="hero">
      <h2>Welcome — this computer can now run apps in <b>safe, isolated boxes</b>.</h2>
      <p>Mycel downloads an app once, stores every file exactly once, and runs it without
         touching the rest of your system. Pick an app below to try it.</p>
      <button class="btn primary" @click="install(catalog[0])">Install Nginx — a tiny web server</button>
    </div>

    <skeleton-block v-if="!S.loaded" h="180px" :n="2"></skeleton-block>
    <empty-state v-else-if="S.apiDown" title="Cannot reach the Mycel server">
      Is <code>myc ui</code> still running in your terminal?
    </empty-state>
    <div v-else class="grid-apps">
      <div v-for="entry in catalog" :key="entry.id" class="app-card lift">
        <div class="head">
          <div class="app-icon">{{ entry.icon }}</div>
          <div>
            <div class="t">{{ entry.title }}</div>
            <div class="tag">{{ entry.tag }}</div>
          </div>
        </div>
        <div class="desc">{{ entry.desc }}</div>
        <div v-if="presets(entry)" class="mono" style="color:var(--faint);font-size:11px">
          Start uses: {{ presets(entry) }}
        </div>

        <template v-for="st in [cardState(entry)]">
          <div class="state">
            <status-dot v-if="st.kind === 'running'" kind="green" live></status-dot>
            <status-dot v-else-if="st.kind === 'installing' || st.kind === 'starting'" kind="amber"></status-dot>
            <status-dot v-else kind="gray"></status-dot>
            <span v-if="st.kind === 'running'">Running for {{ upFor(st.c) }}</span>
            <span v-else-if="st.kind === 'installing'">Installing… {{ st.progress }}</span>
            <span v-else-if="st.kind === 'starting'">Starting…</span>
            <span v-else-if="st.kind === 'installed'">Installed, not running</span>
            <span v-else>Not installed</span>
          </div>
          <div class="actions">
            <template v-if="st.kind === 'running'">
              <!-- st.c.port is the port that actually answers (it may have
                   been remapped when the usual one was busy) -->
              <a v-if="st.c.port || entry.port"
                 :href="'http://localhost:' + (st.c.port || entry.port)" target="_blank">
                <button class="btn sm success">Open ↗</button>
              </a>
              <button class="btn sm ghost" @click="logs(st.c, entry.title)">Logs</button>
              <button class="btn sm danger" @click="stop(st.c, entry.title)">Stop</button>
            </template>
            <button v-else-if="st.kind === 'installing'" class="btn sm" disabled>Installing…</button>
            <button v-else-if="st.kind === 'starting'" class="btn sm" disabled>Starting…</button>
            <template v-else-if="st.kind === 'installed'">
              <button class="btn sm primary" @click="start(entry)">Start</button>
              <button class="btn sm ghost" title="pick a name, port and env vars" @click="customizing = entry">Customize…</button>
            </template>
            <button v-else class="btn sm" @click="install(entry)">Install</button>
          </div>
        </template>
      </div>
    </div>

    <Transition name="fade">
      <start-dialog v-if="customizing" :entry="customizing" :key="customizing.id"
        @start="opts => start(customizing, opts)" @close="customizing = null"></start-dialog>
    </Transition>

    <div class="card" style="margin-top:22px">
      <div class="row spread">
        <div>
          <b>Looking for something else?</b>
          <div style="color:var(--muted);font-size:13px;margin-top:3px">Fetch any app by name from any registry — Docker Hub, GHCR, Quay and friends.</div>
        </div>
        <a href="#ingest"><button class="btn ghost">Get any app →</button></a>
      </div>
    </div>
  </section>`,
};
