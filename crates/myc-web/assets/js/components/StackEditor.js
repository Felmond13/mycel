// Visual stack builder (simple form per service) with an "advanced" toggle
// that switches to raw mycel.toml editing.

import { S } from "../store.js";
import { api } from "../api.js";
import { toast } from "../ui.js";
import { CATALOG } from "../catalog.js";

function emptyService() {
  return { name: "", image: "", command: "", env: [], depends_on: [], workdir: "" };
}

export default {
  name: "StackEditor",
  props: { arg: { type: String, required: true } },
  data() {
    return {
      loaded: false,
      advanced: false,
      toml: "",
      services: [],   // {name, image, command, env: [[k,v]], depends_on: [], workdir}
      saveHint: "",
      saving: false,
    };
  },
  computed: {
    names() { return this.services.map(s => s.name.trim()).filter(Boolean); },
    suggestions() {
      const set = new Set(CATALOG.map(c => c.image));
      S.envs.forEach(e => (e.refs || []).forEach(r => set.add(r)));
      return Array.from(set);
    },
  },
  async mounted() {
    try {
      const d = await api.get("/api/stacks/" + encodeURIComponent(this.arg));
      this.toml = d.toml;
      this.services = d.services.map(s => ({
        name: s.name,
        image: s.image,
        command: s.command.join(" "),
        env: s.env.map(kv => { const i = kv.indexOf("="); return [kv.slice(0, i), kv.slice(i + 1)]; }),
        depends_on: s.depends_on.slice(),
        workdir: s.workdir || "",
      }));
    } catch (_) {
      // brand-new stack: start with one empty service
      this.services = [emptyService()];
    }
    this.loaded = true;
  },
  methods: {
    addService() { this.services.push(emptyService()); },
    removeService(i) { this.services.splice(i, 1); },
    addEnv(s) { s.env.push(["", ""]); },
    rmEnv(s, j) { s.env.splice(j, 1); },
    othersOf(s) { return this.names.filter(n => n !== s.name.trim()); },
    toggleDep(s, dep, ev) {
      if (ev.target.checked) { if (!s.depends_on.includes(dep)) s.depends_on.push(dep); }
      else { const k = s.depends_on.indexOf(dep); if (k !== -1) s.depends_on.splice(k, 1); }
    },
    toggleAdvanced() {
      if (!this.advanced) this.toml = this.generateToml();
      this.advanced = !this.advanced;
    },
    /* Approximate client-side TOML (the server rewrites it canonically on save). */
    generateToml() {
      let out = "[project]\nname = " + JSON.stringify(this.arg) + "\n";
      for (const s of this.services) {
        if (!s.name.trim()) continue;
        out += "\n[services." + s.name.trim() + "]\nimage = " + JSON.stringify(s.image.trim()) + "\n";
        const cmd = s.command.trim() ? s.command.trim().split(/\s+/) : [];
        if (cmd.length) out += "command = [" + cmd.map(c => JSON.stringify(c)).join(", ") + "]\n";
        const env = s.env.filter(kv => kv[0].trim()).map(kv => kv[0].trim() + "=" + kv[1]);
        if (env.length) out += "env = [" + env.map(e => JSON.stringify(e)).join(", ") + "]\n";
        if (s.depends_on.length) out += "depends_on = [" + s.depends_on.map(d => JSON.stringify(d)).join(", ") + "]\n";
        if (s.workdir.trim()) out += "workdir = " + JSON.stringify(s.workdir.trim()) + "\n";
      }
      return out;
    },
    async save() {
      this.saving = true;
      this.saveHint = "";
      try {
        if (this.advanced) {
          await api.put("/api/stacks/" + encodeURIComponent(this.arg), { toml: this.toml });
        } else {
          const services = {};
          for (const s of this.services) {
            const name = s.name.trim().toLowerCase();
            if (!name) throw new Error("every app needs a name (e.g. \u201cweb\u201d)");
            services[name] = {
              image: s.image.trim(),
              command: s.command.trim() ? s.command.trim().split(/\s+/) : [],
              env: s.env.filter(kv => kv[0].trim()).map(kv => kv[0].trim() + "=" + kv[1]),
              depends_on: s.depends_on,
              workdir: s.workdir.trim() || null,
            };
          }
          await api.put("/api/stacks/" + encodeURIComponent(this.arg), { services });
        }
        toast("Stack saved");
        location.hash = "stack/" + encodeURIComponent(this.arg);
      } catch (e) {
        this.saveHint = e.message;
        toast(e.message, true);
      }
      this.saving = false;
    },
  },
  template: `
  <section>
    <a class="back" href="#stacks">← all stacks</a>
    <div class="row spread" style="margin-bottom:6px">
      <h1 class="title" style="margin:0">Stack: <b>{{ arg }}</b></h1>
      <div class="row">
        <span style="color:var(--muted);font-size:13px">Advanced (edit as text)</span>
        <span class="toggle" :class="{ on: advanced }" @click="toggleAdvanced"></span>
      </div>
    </div>
    <div class="sub">Add the apps this stack needs. Mycel starts them in the right order every time.</div>

    <skeleton-block v-if="!loaded" h="200px" :n="1"></skeleton-block>
    <template v-else>
      <datalist id="image-suggestions">
        <option v-for="s in suggestions" :key="s" :value="s"></option>
      </datalist>

      <div v-if="!advanced">
        <div v-for="(s, i) in services" :key="i" class="builder-svc">
          <div class="row spread" style="margin-bottom:12px">
            <b>App {{ i + 1 }}</b>
            <button v-if="services.length > 1" class="btn sm danger" @click="removeService(i)">Remove</button>
          </div>
          <div class="field">
            <span class="label">Name (short, e.g. \u201cweb\u201d or \u201cdb\u201d)</span>
            <input type="text" v-model="s.name" spellcheck="false" style="width:220px">
          </div>
          <div class="field">
            <span class="label">App (pick a suggestion or type any name, e.g. redis:7-alpine)</span>
            <input type="text" v-model="s.image" list="image-suggestions" spellcheck="false" style="width:100%">
          </div>
          <div class="field">
            <span class="label">Command — optional, leave empty for the app's default</span>
            <input type="text" v-model="s.command" spellcheck="false" style="width:100%" class="mono">
          </div>
          <div class="field">
            <span class="label">Settings (environment variables)</span>
            <div v-for="(kv, j) in s.env" :key="j" class="kvrow">
              <input type="text" placeholder="NAME" v-model="kv[0]" spellcheck="false">
              <input type="text" placeholder="value" v-model="kv[1]" spellcheck="false">
              <button class="btn sm ghost" @click="rmEnv(s, j)" title="remove">✕</button>
            </div>
            <button class="btn sm ghost" @click="addEnv(s)">+ add setting</button>
          </div>
          <div class="field" style="margin-bottom:0">
            <span class="label">Starts after (dependencies)</span>
            <template v-if="othersOf(s).length">
              <label v-for="n in othersOf(s)" :key="n" class="depbox">
                <input type="checkbox" :checked="s.depends_on.includes(n)" @change="toggleDep(s, n, $event)">
                {{ n }}
              </label>
            </template>
            <span v-else style="color:var(--faint);font-size:12.5px">add more apps to declare an order</span>
          </div>
        </div>
        <div class="row" style="margin-bottom:20px">
          <button class="btn ghost" @click="addService">+ Add another app</button>
        </div>
      </div>

      <div v-else class="card">
        <div style="color:var(--muted);font-size:13px;margin-bottom:10px">
          This is the stack as a <code>mycel.toml</code> file — the same format the <code>myc up</code> command uses.
        </div>
        <textarea v-model="toml" rows="18" spellcheck="false"></textarea>
      </div>

      <div class="row">
        <button class="btn primary" :disabled="saving" @click="save">Save stack</button>
        <a href="#stacks"><button class="btn ghost">Cancel</button></a>
        <span v-if="saveHint" style="color:var(--red);font-size:13px">{{ saveHint }}</span>
      </div>
    </template>
  </section>`,
};
