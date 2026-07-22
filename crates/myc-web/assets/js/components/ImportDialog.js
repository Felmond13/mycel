// "Import" dialog for the Library: bring an app in from a .mycel file
// (drag-and-drop or file picker, with upload progress) or fetch it from a
// hub by name — only the files this machine is missing are downloaded.

import { api } from "../api.js";
import { refreshState, refreshStats } from "../store.js";
import { fmtBytes, shortRef } from "../format.js";

const HUB_URL_KEY = "mycel.hubUrl";

export default {
  name: "ImportDialog",
  emits: ["close"],
  data() {
    return {
      dragging: false,
      uploading: false,
      pct: null,        // upload progress 0-100, null = waiting for server
      imported: null,   // /api/import result
      hubUrl: localStorage.getItem(HUB_URL_KEY) || "",
      hubRef: "",
      pulling: false,
      pulled: null,     // /api/hub/pull result
      fail: "",
    };
  },
  computed: {
    pulledText() {
      const r = this.pulled;
      if (!r) return "";
      let txt = shortRef(r.ref) + " is ready to launch — downloaded " + r.blobs_downloaded
        + " of " + r.blobs_total + " files (" + fmtBytes(r.bytes_downloaded) + ")";
      if (r.blobs_already_present) txt += "; the other " + r.blobs_already_present + " were already on this machine";
      return txt + ".";
    },
  },
  methods: {
    shortRef,
    pick() { this.$refs.file.click(); },
    onPick(ev) {
      const f = ev.target.files && ev.target.files[0];
      if (f) this.upload(f);
      ev.target.value = "";
    },
    onDrop(ev) {
      this.dragging = false;
      const f = ev.dataTransfer.files && ev.dataTransfer.files[0];
      if (f) this.upload(f);
    },
    upload(file) {
      this.uploading = true;
      this.imported = null;
      this.pulled = null;
      this.fail = "";
      this.pct = 0;
      // XMLHttpRequest instead of fetch: it reports upload progress.
      const xhr = new XMLHttpRequest();
      xhr.open("POST", "/api/import");
      xhr.setRequestHeader("Content-Type", "application/octet-stream");
      xhr.upload.onprogress = (e) => {
        this.pct = e.lengthComputable ? Math.round(100 * e.loaded / e.total) : null;
      };
      xhr.onload = () => {
        this.uploading = false;
        this.pct = null;
        let body = null;
        try { body = JSON.parse(xhr.responseText); } catch (_) { /* non-JSON error */ }
        if (xhr.status >= 200 && xhr.status < 300) {
          this.imported = body;
          refreshState(); refreshStats();
        } else {
          this.fail = (body && body.error) || ("upload failed (" + xhr.status + ")");
        }
      };
      xhr.onerror = () => {
        this.uploading = false;
        this.pct = null;
        this.fail = "upload failed — is the dashboard still running?";
      };
      xhr.send(file);
    },
    async pull() {
      const url = this.hubUrl.trim();
      const ref = this.hubRef.trim();
      if (!url) { this.fail = "Type the hub address first, e.g. http://hub:9600"; return; }
      if (!ref) { this.fail = "Which app? Type its name, e.g. redis:7-alpine"; return; }
      localStorage.setItem(HUB_URL_KEY, url);
      this.pulling = true;
      this.imported = null;
      this.pulled = null;
      this.fail = "";
      try {
        this.pulled = await api.post("/api/hub/pull", { ref, url });
        refreshState(); refreshStats();
      } catch (e) {
        this.fail = e.message;
      }
      this.pulling = false;
    },
  },
  template: `
  <div class="overlay" @click.self="$emit('close')">
    <div class="dialog" style="width:min(560px,92vw)">
      <h3>Import an app</h3>
      <p style="margin-bottom:16px">Got a <b>.mycel</b> file from someone, or a hub on your network? Bring the app in here.</p>

      <span class="label">From a file</span>
      <div @dragover.prevent="dragging = true" @dragleave.prevent="dragging = false" @drop.prevent="onDrop"
           @click="pick" role="button"
           :style="'margin:6px 0 6px;padding:26px;text-align:center;cursor:pointer;border-radius:12px;border:1.5px dashed '
                   + (dragging ? 'var(--teal)' : 'var(--border)') + ';color:var(--muted);font-size:13px'
                   + (dragging ? ';background:var(--teal-soft)' : '')">
        <template v-if="uploading">
          <span class="spinner"></span>
          {{ pct === null ? 'Checking and storing the files…' : 'Uploading… ' + pct + '%' }}
        </template>
        <template v-else>Drop a .mycel file here, or click to choose one</template>
      </div>
      <input ref="file" type="file" accept=".mycel,.gz,.tar.gz,application/gzip" style="display:none" @change="onPick">
      <div v-if="imported" style="color:var(--green);font-size:13px;margin-bottom:12px">
        ✓ {{ shortRef(imported.name) }} is ready to launch — {{ imported.blobs }} files checked and stored.
        <a :href="'#env/' + encodeURIComponent(imported.id)" @click="$emit('close')">take a look →</a>
      </div>

      <span class="label" style="margin-top:8px">Get from a hub</span>
      <p style="margin:4px 0 10px;color:var(--muted);font-size:13px">
        Ask a hub (<span class="mono">myc hub serve</span>) for an app by name. Files you already
        have are never downloaded twice.
      </p>
      <div class="row">
        <input type="text" v-model="hubUrl" placeholder="http://hub:9600" style="flex:1;min-width:170px" spellcheck="false">
        <input type="text" v-model="hubRef" @keydown.enter="pull" placeholder="app name — e.g. redis:7-alpine" style="flex:1;min-width:170px" spellcheck="false">
        <button class="btn" :disabled="pulling" @click="pull">
          <span v-if="pulling" class="spinner"></span>{{ pulling ? 'Getting…' : 'Get it' }}
        </button>
      </div>
      <div v-if="pulled" style="color:var(--green);font-size:13px;margin-top:10px">✓ {{ pulledText }}</div>

      <div v-if="fail" style="color:var(--red);font-size:13px;margin-top:10px">✗ {{ fail }}</div>

      <div class="row" style="justify-content:flex-end;margin-top:18px">
        <button class="btn ghost" @click="$emit('close')">Close</button>
      </div>
    </div>
  </div>`,
};
