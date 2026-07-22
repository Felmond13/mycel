// Full-height slide-over showing a container's log, tail-polled by ui.js.
// Header carries the app avatar + name and a clearly stateful follow toggle.

import { UI, closeLogs } from "../ui.js";
import { S } from "../store.js";
import { historyFor, fmtCpu } from "../metrics.js";
import { fmtBytes } from "../format.js";

export default {
  name: "LogsPanel",
  computed: {
    logs() { return UI.logs; },
    statusClass() {
      if (this.logs.running) return "green";
      return this.logs.exit === 0 ? "gray" : "red";
    },
    /** Live container object behind the drawer (for current CPU/RAM). */
    live() { return S.containers.find(c => c.id === this.logs.cid && c.running); },
    hist() { return historyFor(this.logs.cid); },
  },
  methods: {
    closeLogs, fmtCpu, fmtBytes,
    toggleFollow() { UI.logs.follow = !UI.logs.follow; },
  },
  template: `
  <Transition name="drawer">
    <div v-if="logs.open" class="drawer-wrap">
      <div class="drawer-back" @click="closeLogs"></div>
      <div class="drawer">
        <div class="drawer-head">
          <ref-avatar v-if="logs.reference" :reference="logs.reference" size="30"></ref-avatar>
          <div class="who">
            <b>{{ logs.title }}</b>
            <ref-name v-if="logs.reference" :reference="logs.reference" :bold="false"></ref-name>
          </div>
          <span class="pill" :class="statusClass">{{ logs.status }}</span>
          <span class="spacer"></span>
          <button class="btn sm" :class="logs.follow ? 'success' : 'ghost'" @click="toggleFollow"
                  :title="logs.follow ? 'New output scrolls into view' : 'View stays where you scrolled'">
            {{ logs.follow ? '● Following' : '○ Follow' }}
          </button>
          <button class="btn sm ghost" @click="closeLogs">Close</button>
        </div>
        <div v-if="live && hist.cpu.length > 1" class="drawer-sparks">
          <div class="spark-box">
            <div class="k">CPU <span class="mono">{{ fmtCpu(live.cpu_percent) }}</span></div>
            <spark-line :values="hist.cpu" :max="100" color="var(--teal)" :height="24"></spark-line>
          </div>
          <div class="spark-box">
            <div class="k">RAM <span class="mono">{{ fmtBytes(live.memory_bytes) }}</span></div>
            <spark-line :values="hist.mem" color="var(--violet)" :height="24"></spark-line>
          </div>
        </div>
        <pre class="drawer-body">{{ logs.text || '(no output yet)' }}</pre>
      </div>
    </div>
  </Transition>`,
};
