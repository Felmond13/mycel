// Minimal hash router: `#page` or `#page/arg`. The current route is
// reactive; AppShell renders the matching component.

import { reactive } from "./vue.js";

/** page name -> globally registered component name */
export const PAGES = {
  apps: "apps-page",
  running: "containers-page",
  stacks: "stacks-page",
  stack: "stack-view",
  stackedit: "stack-editor",
  library: "library-page",
  environments: "library-page", // legacy bookmarks
  env: "library-detail",
  store: "store-page",
  ingest: "ingest-page",
  diff: "diff-page",
  search: "search-page",
  doctor: "doctor-page",
};

/** Sidebar highlight: detail pages light up their parent section. */
const NAV_ALIAS = { env: "library", environments: "library", stack: "stacks", stackedit: "stacks" };

export const route = reactive({ page: "apps", arg: "" });

function parse() {
  const hash = location.hash.slice(1) || "apps";
  const slash = hash.indexOf("/");
  const page = slash === -1 ? hash : hash.slice(0, slash);
  const arg = slash === -1 ? "" : decodeURIComponent(hash.slice(slash + 1));
  if (PAGES[page]) {
    route.page = page;
    route.arg = arg;
  } else {
    route.page = "apps";
    route.arg = "";
  }
}

export function navKey() {
  return NAV_ALIAS[route.page] || route.page;
}

export function startRouter() {
  window.addEventListener("hashchange", parse);
  parse();
}
