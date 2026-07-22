// Slim navigation sidebar: logo, main pages, Advanced group, store footer.

import { S } from "../store.js";
import { navKey } from "../router.js";
import { fmtBytes } from "../format.js";

export default {
  name: "SideBar",
  computed: {
    active() { return navKey(); },
    runningCount() { return S.containers.filter(c => c.running).length; },
    footText() {
      if (!S.stats) return "…";
      return this.runningCount + " running · " + fmtBytes(S.stats.physical_bytes) + " on disk";
    },
  },
  template: `
  <nav class="side">
    <div class="logo">
      <svg width="27" height="27" viewBox="0 0 26 26" fill="none">
        <circle cx="6" cy="20" r="2.6" fill="#34e2cd"/>
        <circle cx="13" cy="6" r="2.6" fill="#59a8f5"/>
        <circle cx="21" cy="17" r="2.6" fill="#8b7cf6"/>
        <path d="M7.5 18.2 L11.8 8.2 M14.8 7.6 L19.4 15.4 M8.4 19.6 L18.4 17.4"
              stroke="#59a8f5" stroke-width="1.4" stroke-linecap="round" opacity=".55"/>
      </svg>
      <div><span class="word">mycel</span><small>your apps, no hassle</small></div>
    </div>

    <a class="nav-item" :class="{ active: active === 'apps' }" href="#apps">
      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><rect x="2" y="2" width="5" height="5" rx="1.2"/><rect x="9" y="2" width="5" height="5" rx="1.2"/><rect x="2" y="9" width="5" height="5" rx="1.2"/><path d="M11.5 9v5M9 11.5h5"/></svg>
      <span class="nav-text">Apps</span>
    </a>
    <a class="nav-item" :class="{ active: active === 'ingest' }" href="#ingest">
      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><path d="M8 2v8M4.5 6.5 8 10l3.5-3.5"/><path d="M2.5 12.5h11"/></svg>
      <span class="nav-text">Get any app</span>
    </a>
    <a class="nav-item" :class="{ active: active === 'running' }" href="#running">
      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><circle cx="8" cy="8" r="6"/><path d="M6.5 5.5v5l4-2.5z" fill="currentColor" stroke="none"/></svg>
      <span class="nav-text">Running</span>
      <span v-if="runningCount" class="nav-badge">{{ runningCount }}</span>
    </a>
    <a class="nav-item" :class="{ active: active === 'library' }" href="#library">
      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><path d="M2.5 3.5h3v10h-3zM6.5 3.5h3v10h-3z"/><path d="m10.5 4 2.8-.7 2 9.7-2.9.7z"/></svg>
      <span class="nav-text">Library</span>
    </a>
    <a class="nav-item" :class="{ active: active === 'stacks' }" href="#stacks">
      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><path d="M8 2 14 5 8 8 2 5z"/><path d="M2 8.5 8 11.5 14 8.5"/><path d="M2 11.5 8 14.5 14 11.5"/></svg>
      <span class="nav-text">Stacks</span>
    </a>

    <div class="nav-label">Advanced</div>
    <a class="nav-item" :class="{ active: active === 'store' }" href="#store">
      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><ellipse cx="8" cy="4" rx="5.5" ry="2.2"/><path d="M2.5 4v8c0 1.2 2.5 2.2 5.5 2.2s5.5-1 5.5-2.2V4"/><path d="M2.5 8c0 1.2 2.5 2.2 5.5 2.2s5.5-1 5.5-2.2"/></svg>
      <span class="nav-text">Store</span>
    </a>
    <a class="nav-item" :class="{ active: active === 'diff' }" href="#diff">
      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><circle cx="4.5" cy="4" r="2"/><circle cx="4.5" cy="12" r="2"/><circle cx="11.5" cy="8" r="2"/><path d="M4.5 6v4M6.3 4.8 9.8 7M6.3 11.2 9.8 9"/></svg>
      <span class="nav-text">Compare</span>
    </a>
    <a class="nav-item" :class="{ active: active === 'search' }" href="#search">
      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5 14 14"/></svg>
      <span class="nav-text">Find a file</span>
    </a>
    <a class="nav-item" :class="{ active: active === 'doctor' }" href="#doctor">
      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><path d="M8 14s-5.5-3.2-5.5-7A3.2 3.2 0 0 1 8 4.6 3.2 3.2 0 0 1 13.5 7c0 3.8-5.5 7-5.5 7Z"/><path d="M4.5 8h2l1-1.8 1.2 3 1-1.2h1.8"/></svg>
      <span class="nav-text">Doctor</span>
    </a>

    <div class="nav-foot">local dashboard<br><code>{{ footText }}</code></div>
  </nav>`,
};
