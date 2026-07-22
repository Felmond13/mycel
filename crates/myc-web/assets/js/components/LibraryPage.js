// The Library: everything installed locally, one row per app, with an
// identicon avatar, tag badge, platform chip, pin toggle and removal.

import { S, refreshState, refreshStats } from "../store.js";
import { api } from "../api.js";
import { toast, confirmDialog } from "../ui.js";
import { fmtBytes, shortRef } from "../format.js";

export default {
  name: "LibraryPage",
  data() {
    return {
      shareEnv: null,    // env being shared (ShareDialog open)
      importOpen: false, // ImportDialog open
    };
  },
  computed: {
    S() { return S; },
  },
  methods: {
    fmtBytes,
    open(e) { location.hash = "env/" + encodeURIComponent(e.id); },
    async togglePin(e) {
      try {
        await api.post("/api/envs/" + encodeURIComponent(e.id) + "/" + (e.pinned ? "unpin" : "pin"));
        toast(e.pinned ? shortRef(e.name) + " unpinned" : shortRef(e.name) + " pinned — cleanup will never touch it");
        e.pinned = !e.pinned;
      } catch (err) { toast(err.message, true); }
    },
    async remove(e) {
      const ok = await confirmDialog("Remove " + shortRef(e.name) + " from your library?",
        "Its files stay on disk until you run cleanup on the Store page. You can get it again any time.", "Remove");
      if (!ok) return;
      try {
        await api.del("/api/envs/" + encodeURIComponent(e.id));
        toast("Removed. Run cleanup on the Store page to reclaim space.");
        refreshState(); refreshStats();
      } catch (err) { toast(err.message, true); }
    },
  },
  template: `
  <section>
    <div class="row spread" style="align-items:flex-start">
      <div>
        <h1 class="title">Library</h1>
        <div class="sub">The software installed on this machine. Each app is a precise list of files, and every file is stored once.</div>
      </div>
      <button class="btn" @click="importOpen = true" title="Import an app from a .mycel file or a hub">
        <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" style="margin-right:6px;vertical-align:-2px">
          <path d="M8 2v8m0 0 3-3M8 10 5 7M3 12.5h10"/>
        </svg>Import
      </button>
    </div>

    <skeleton-block v-if="!S.loaded" h="66px" :n="4"></skeleton-block>
    <empty-state v-else-if="!S.envs.length" title="Your library is empty">
      Get any app from any registry — it takes a few seconds and lands here.
      <div><a href="#ingest"><button class="btn primary">Get your first app →</button></a></div>
    </empty-state>
    <template v-else>
      <div class="card tight">
        <table class="lib">
          <thead><tr>
            <th>App</th><th>Platform</th>
            <th class="num">Files</th><th class="num">Size</th>
            <th class="ctr">Pinned</th><th></th><th></th>
          </tr></thead>
          <tbody>
            <tr v-for="e in S.envs" :key="e.id" class="click" @click="open(e)">
              <td>
                <div class="lib-app">
                  <ref-avatar :reference="e.name" size="34"></ref-avatar>
                  <ref-name :reference="e.name"></ref-name>
                </div>
              </td>
              <td><span class="pill gray">{{ e.os }}/{{ e.arch }}</span></td>
              <td class="num">{{ e.files.toLocaleString('en-US') }}</td>
              <td class="num">{{ fmtBytes(e.logical_size) }}</td>
              <td class="ctr" @click.stop>
                <button class="iconbtn" :class="{ on: e.pinned }" @click="togglePin(e)"
                        :title="e.pinned ? 'Pinned — protected from cleanup' : 'Pin to protect from cleanup'"
                        :aria-pressed="e.pinned">
                  <svg viewBox="0 0 16 16" :fill="e.pinned ? 'currentColor' : 'none'" stroke="currentColor" stroke-width="1.4">
                    <path d="M9.5 2 14 6.5l-3.2.7-.6 3.1L8 8.1l-3.6 4.7-1.2-1.2L7.9 8 5.7 5.8l3.1-.6z"/>
                  </svg>
                </button>
              </td>
              <td class="ctr" @click.stop>
                <button class="iconbtn" @click="shareEnv = e" title="Share — download as a file or send to a hub">
                  <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4">
                    <circle cx="4" cy="8" r="1.8"/><circle cx="12" cy="3.5" r="1.8"/><circle cx="12" cy="12.5" r="1.8"/>
                    <path d="M5.6 7.1l4.8-2.7M5.6 8.9l4.8 2.7"/>
                  </svg>
                </button>
              </td>
              <td class="ctr" @click.stop>
                <button class="iconbtn danger" @click="remove(e)" title="Remove from library">
                  <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4">
                    <path d="M3 4.5h10M6.5 4.5v-1a1 1 0 0 1 1-1h1a1 1 0 0 1 1 1v1M5 4.5l.6 8a1 1 0 0 0 1 .9h2.8a1 1 0 0 0 1-.9l.6-8"/>
                  </svg>
                </button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <div class="sub">Click an app to see its files, run commands and package list.</div>
    </template>

    <share-dialog v-if="shareEnv" :env="shareEnv" @close="shareEnv = null"></share-dialog>
    <import-dialog v-if="importOpen" @close="importOpen = false"></import-dialog>
  </section>`,
};
