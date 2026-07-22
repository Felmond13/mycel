// "Share" dialog for one library app: download it as a portable .mycel
// file, or send it to a hub (only the files the hub is missing travel —
// that's the headline, so we show the numbers proudly).

import { api } from "../api.js";
import { fmtBytes, shortRef } from "../format.js";

const HUB_URL_KEY = "mycel.hubUrl";

export default {
  name: "ShareDialog",
  props: { env: { type: Object, required: true } },
  emits: ["close"],
  data() {
    return {
      hubUrl: localStorage.getItem(HUB_URL_KEY) || "",
      sending: false,
      sent: null,  // push result JSON
      fail: "",
    };
  },
  computed: {
    title() { return shortRef(this.env.name); },
    exportHref() { return "/api/export/" + encodeURIComponent(this.env.id); },
    sentText() {
      const r = this.sent;
      if (!r) return "";
      if (!r.blobs_uploaded) {
        return "Nothing to send — the hub already had every one of the " + r.blobs_total + " files. Zero bytes moved.";
      }
      let txt = "Sent " + r.blobs_uploaded + " of " + r.blobs_total + " files (" + fmtBytes(r.bytes_uploaded) + ")";
      if (r.blobs_already_present) txt += " — the other " + r.blobs_already_present + " were already on the hub, so they never left this machine";
      return txt + ".";
    },
  },
  methods: {
    fmtBytes,
    async send() {
      const url = this.hubUrl.trim();
      if (!url) { this.fail = "Type the hub address first, e.g. http://hub:9600"; return; }
      localStorage.setItem(HUB_URL_KEY, url);
      this.sending = true;
      this.sent = null;
      this.fail = "";
      try {
        this.sent = await api.post("/api/hub/push", { ref: this.env.name, url });
      } catch (e) {
        this.fail = e.message;
      }
      this.sending = false;
    },
  },
  template: `
  <div class="overlay" @click.self="$emit('close')">
    <div class="dialog" style="width:min(520px,92vw)">
      <h3>Share {{ title }}</h3>
      <p style="margin-bottom:16px">Two ways to hand this app to someone else — pick whichever fits.</p>

      <span class="label">Download as a file</span>
      <p style="margin:4px 0 10px;color:var(--muted);font-size:13px">
        One self-contained <b>.mycel</b> file. Send it to anyone — over chat, a USB stick,
        whatever — and they can import it into their own library. No internet needed on their side.
      </p>
      <a :href="exportHref" download><button class="btn primary" style="margin-bottom:18px">Download .mycel file</button></a>

      <span class="label">Send to a hub</span>
      <p style="margin:4px 0 10px;color:var(--muted);font-size:13px">
        A hub is a shared Mycel store (<span class="mono">myc hub serve</span>). Because every file
        is content-addressed, only the files the hub doesn't already have are sent.
      </p>
      <div class="row">
        <input type="text" v-model="hubUrl" @keydown.enter="send"
               placeholder="http://hub:9600" style="flex:1;min-width:200px" spellcheck="false">
        <button class="btn" :disabled="sending" @click="send">
          <span v-if="sending" class="spinner"></span>{{ sending ? 'Sending…' : 'Send' }}
        </button>
      </div>
      <div v-if="sent" style="color:var(--green);font-size:13px;margin-top:10px">✓ {{ sentText }}</div>
      <div v-if="fail" style="color:var(--red);font-size:13px;margin-top:10px">✗ {{ fail }}</div>

      <div class="row" style="justify-content:flex-end;margin-top:18px">
        <button class="btn ghost" @click="$emit('close')">Close</button>
      </div>
    </div>
  </div>`,
};
