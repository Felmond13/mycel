// Health checks with pass/fail pills and actionable fixes.

import { api } from "../api.js";

export default {
  name: "DoctorPage",
  data() { return { d: null, error: "", rerunning: false }; },
  mounted() { this.run(); },
  methods: {
    async run() {
      this.rerunning = true;
      try { this.d = await api.get("/api/doctor"); this.error = ""; }
      catch (e) { this.error = e.message; }
      this.rerunning = false;
    },
  },
  template: `
  <section>
    <h1 class="title">Doctor</h1>
    <div class="sub">Checks that this machine can get and run apps, with a fix for anything broken.</div>

    <empty-state v-if="error" :title="error"></empty-state>
    <skeleton-block v-else-if="!d" h="60px" :n="5"></skeleton-block>
    <div v-else class="card">
      <div v-for="c in d.checks" :key="c.name" class="check">
        <span class="pill" :class="c.ok ? 'green' : 'red'">{{ c.ok ? 'PASS' : 'FAIL' }}</span>
        <div>
          <b>{{ c.name }}</b> — <span style="color:var(--muted)">{{ c.detail }}</span>
          <div v-if="c.fix" class="fix">fix: <code>{{ c.fix }}</code></div>
        </div>
      </div>
      <div class="row spread" style="margin-top:14px">
        <span style="color:var(--muted);font-size:13px">
          {{ d.ok ? 'Everything works — this machine can get and run apps.'
                  : 'Fix the failing checks above, then run the checks again.' }}
        </span>
        <button class="btn sm ghost" :disabled="rerunning" @click="run">
          <span v-if="rerunning" class="spinner"></span>Run checks again
        </button>
      </div>
    </div>
  </section>`,
};
